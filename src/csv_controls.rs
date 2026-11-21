//! Delimited text import options and bounded, cancellable preview.
use super::*;
use crate::controls;
use crate::engine::csv_source::{CsvColumnType, CsvOptions, CsvProbe, valid_delimiter};

const MAX_INFERENCE_ROWS: usize = 100_000;
const PREVIEW_COLUMNS: usize = 5;
const PREVIEW_ROWS: usize = 6;
const PREVIEW_CELL_HEIGHT: f32 = 23.;
// Include the name and type rows and the grid's top and bottom borders.
const PREVIEW_GRID_HEIGHT: f32 = (PREVIEW_ROWS + 2) as f32 * PREVIEW_CELL_HEIGHT + 2.;

fn read_inference_rows(input: &Entity<TextInput>, cx: &App) -> Option<usize> {
    input
        .read(cx)
        .text()
        .trim()
        .parse()
        .ok()
        .filter(|records| (1..=MAX_INFERENCE_ROWS).contains(records))
}

fn inference_range_label() -> String {
    t("Enter a whole number from 1 to {limit}.").replace("{limit}", &MAX_INFERENCE_ROWS.to_string())
}

const COMMON_DELIMITERS: [u8; 7] = [b',', b'\t', b';', b'|', b' ', b':', b'^'];

fn csv_delimiters() -> impl Iterator<Item = u8> {
    COMMON_DELIMITERS.into_iter().chain(
        (b' '..=b'~').filter(|byte| valid_delimiter(*byte) && !COMMON_DELIMITERS.contains(byte)),
    )
}

fn csv_delimiter_label(delimiter: u8) -> String {
    let (name, symbol) = match delimiter {
        b',' => ("Comma", ","),
        b'\t' => ("Tab", "⇥"),
        b';' => ("Semicolon", ";"),
        b'|' => ("Pipe", "|"),
        b' ' => ("Space", "␣"),
        b':' => ("Colon", ":"),
        b'^' => ("Caret", "^"),
        other => return char::from(other).to_string(),
    };
    format!("{}  {symbol}", t(name))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ImportMode {
    Open,
    Reload,
    ResetView,
}

pub(super) struct CsvImport {
    path: PathBuf,
    options: CsvOptions,
    mode: ImportMode,
    previous_types: HashMap<String, CsvColumnType>,
    detect_delimiter: bool,
    delimiter_menu: Option<u8>,
    delimiter_focus: FocusHandle,
    delimiter_scroll: gpui::UniformListScrollHandle,
    sample_size: Entity<TextInput>,
    preview: Option<CsvProbe>,
    pending: Option<Arc<AtomicBool>>,
    error: Option<String>,
    load_error: Option<String>,
    first_column: usize,
    type_column: Option<usize>,
    _sample_subscription: Subscription,
}

impl Drop for CsvImport {
    fn drop(&mut self) {
        if let Some(pending) = &self.pending {
            pending.store(true, Ordering::Relaxed);
        }
    }
}

impl CsvImport {
    fn highlight_delimiter(&mut self, index: usize, byte: u8) {
        self.delimiter_menu = Some(byte);
        self.delimiter_scroll
            .scroll_to_item(index, gpui::ScrollStrategy::Center);
    }

    fn sample_records(&self, cx: &App) -> Option<usize> {
        read_inference_rows(&self.sample_size, cx)
    }

    fn preview_current(&self, cx: &App) -> bool {
        self.preview_visible(cx)
            && self.error.is_none()
            && self
                .preview
                .as_ref()
                .is_some_and(|preview| preview.options == self.options)
    }

    fn preview_visible(&self, cx: &App) -> bool {
        self.pending.is_none()
            && self.preview.is_some()
            && self.sample_records(cx) == Some(self.options.sample_records)
    }
}

impl Viewer {
    pub(super) fn begin_csv_import(
        &mut self,
        path: PathBuf,
        previous_options: Option<CsvOptions>,
        cx: &mut Context<Self>,
    ) {
        self.cancel(cx);
        self.selection_menu = None;
        self.settings_menu = None;
        self.error = None;
        let detect_format = previous_options.is_none();
        let mut options = previous_options.unwrap_or_default();
        let previous_types = std::mem::take(&mut options.column_types)
            .into_iter()
            .filter_map(|(index, kind)| {
                let column = self.info.as_ref()?.columns.get(index)?;
                Some((column.name.clone(), kind))
            })
            .collect();
        let dark = self.preferences.dark;
        let sample_size = cx.new(|cx| {
            let mut input = TextInput::new(cx).with_dark(dark);
            input.set_text(&options.sample_records.to_string(), cx);
            input
        });
        let subscription = cx.subscribe(&sample_size, |this, _, event, cx| match event {
            InputEvent::Changed => cx.notify(),
            InputEvent::Submit => this.apply_csv_preview_options(cx),
        });
        self.csv_import = Some(CsvImport {
            path,
            options,
            previous_types,
            detect_delimiter: detect_format,
            delimiter_menu: None,
            delimiter_focus: cx.focus_handle(),
            delimiter_scroll: gpui::UniformListScrollHandle::new(),
            mode: if !detect_format && self.result.is_some() {
                ImportMode::Reload
            } else {
                ImportMode::Open
            },
            sample_size,
            preview: None,
            pending: None,
            error: None,
            load_error: None,
            first_column: 0,
            type_column: None,
            _sample_subscription: subscription,
        });
        self.panel = Some(Panel::CsvImport);
        self.refresh_csv_preview(cx);
    }

    pub(super) fn dismiss_csv_import(&mut self, cx: &mut Context<Self>) {
        if self.csv_import.take().is_some() && matches!(self.panel, Some(Panel::CsvImport) | None) {
            self.panel = (self.awaiting_initial_range() && !self.selecting_initial_fields)
                .then_some(Panel::Range);
            cx.notify();
        }
    }

    pub(super) fn csv_import_focus(&self) -> FocusHandle {
        match &self.csv_import {
            Some(state) => state.delimiter_focus.clone(),
            None => self.focus.clone(),
        }
    }

    pub(super) fn csv_import_input_focused(&self, window: &Window, cx: &App) -> bool {
        self.panel == Some(Panel::CsvImport)
            && self
                .csv_import
                .as_ref()
                .is_some_and(|state| state.sample_size.focus_handle(cx).is_focused(window))
    }

    pub(super) fn set_csv_import_dark(&self, dark: bool, cx: &mut Context<Self>) {
        if let Some(state) = &self.csv_import {
            state
                .sample_size
                .update(cx, |input, cx| input.set_dark(dark, cx));
        }
    }

    fn refresh_csv_preview(&mut self, cx: &mut Context<Self>) {
        let Some(state) = &mut self.csv_import else {
            return;
        };
        let cancel = Arc::new(AtomicBool::new(false));
        if let Some(previous) = state.pending.replace(cancel.clone()) {
            previous.store(true, Ordering::Relaxed);
        }
        state.error = None;
        state.load_error = None;
        let path = state.path.clone();
        let options = state.options.clone();
        let detect_format = state.detect_delimiter;
        let previous_types = state.previous_types.clone();
        let worker_cancel = cancel.clone();
        let task = cx.background_executor().spawn(async move {
            if detect_format {
                CsvProbe::read_detected(&path, options, &worker_cancel)
            } else if !previous_types.is_empty() {
                CsvProbe::read_for_reload(&path, options, &previous_types, &worker_cancel)
            } else {
                CsvProbe::read(&path, Some(options), &worker_cancel)
            }
        });
        cx.spawn(async move |weak, cx| {
            let result = task.await;
            let _ = weak.update(cx, |this, cx| {
                let Some(state) = &mut this.csv_import else {
                    return;
                };
                if !state
                    .pending
                    .as_ref()
                    .is_some_and(|pending| Arc::ptr_eq(pending, &cancel))
                {
                    return;
                }
                state.pending = None;
                match result {
                    Ok(probe) => {
                        state.previous_types.clear();
                        if state.mode != ImportMode::Open {
                            let same_fields = this.info.as_ref().is_some_and(|info| {
                                info.columns
                                    .iter()
                                    .map(|col| &col.name)
                                    .eq(probe.columns.iter().map(|col| &col.name))
                            });
                            state.mode = if same_fields
                                && this.displayed_filters.to_filter(&probe.columns).is_ok()
                            {
                                ImportMode::Reload
                            } else {
                                ImportMode::ResetView
                            };
                        }
                        state.detect_delimiter = false;
                        state.options = probe.options.clone();
                        let columns = probe.columns.len();
                        if state.first_column >= columns {
                            state.first_column = 0;
                        }
                        state.type_column = state.type_column.filter(|&index| index < columns);
                        state.preview = Some(probe);
                    }
                    Err(error) => {
                        state.error = Some(format!("{error:#}"));
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn apply_csv_preview_options(&mut self, cx: &mut Context<Self>) {
        let Some(state) = &mut self.csv_import else {
            return;
        };
        let Some(records) = state.sample_records(cx) else {
            return;
        };
        state.options.sample_records = records;
        self.refresh_csv_preview(cx);
    }

    fn select_csv_delimiter(&mut self, delimiter: u8, cx: &mut Context<Self>) {
        let Some(state) = &mut self.csv_import else {
            return;
        };
        state.delimiter_menu = None;
        let was_detecting = std::mem::take(&mut state.detect_delimiter);
        if state.options.delimiter != delimiter {
            self.change_csv_structure(|options| options.delimiter = delimiter, cx);
        } else if was_detecting || state.error.is_some() {
            self.refresh_csv_preview(cx);
        } else {
            cx.notify();
        }
    }

    fn change_csv_structure(
        &mut self,
        change: impl FnOnce(&mut CsvOptions),
        cx: &mut Context<Self>,
    ) {
        if let Some(state) = &mut self.csv_import {
            change(&mut state.options);
            if let Some(records) = state.sample_records(cx) {
                state.options.sample_records = records;
            }
            // These overrides address column ordinals in the old structure.
            state.options.column_types.clear();
            state.previous_types.clear();
            state.first_column = 0;
            state.type_column = None;
            state.preview = None;
            self.refresh_csv_preview(cx);
        }
    }

    fn choose_csv_type(
        &mut self,
        index: usize,
        kind: Option<CsvColumnType>,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = &mut self.csv_import else {
            return;
        };
        if !state.preview_visible(cx) {
            return;
        }
        match kind {
            Some(kind) => {
                state.options.column_types.insert(index, kind);
            }
            None => {
                state.options.column_types.remove(&index);
            }
        }
        state.type_column = None;
        self.refresh_csv_preview(cx);
    }

    fn import_csv(&mut self, cx: &mut Context<Self>) {
        if !self
            .csv_import
            .as_ref()
            .is_some_and(|state| state.preview_current(cx))
        {
            return;
        }
        if let Some(state) = &self.csv_import
            && let Some(preview) = &state.preview
        {
            let path = state.path.clone();
            let options = state.options.clone();
            let prepared = preview.prepared();
            if state.mode == ImportMode::Reload {
                self.reload_source(
                    self.displayed_request.clone(),
                    self.displayed_filters.clone(),
                    Some((options, prepared)),
                    cx,
                );
            } else {
                self.open_source(vec![path], Some((options, prepared)), cx);
            }
        }
    }

    pub(super) fn show_csv_load_error(&mut self, error: anyhow::Error, cx: &mut Context<Self>) {
        if let Some(state) = &mut self.csv_import {
            state.load_error = Some(format!("{error:#}"));
            self.panel = Some(Panel::CsvImport);
            cx.notify();
        }
    }

    fn toggle_csv_delimiter_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(state) = &mut self.csv_import else {
            return;
        };
        state.delimiter_focus.focus(window);
        if state.delimiter_menu.take().is_none()
            && let Some(index) = csv_delimiters().position(|byte| byte == state.options.delimiter)
        {
            state.highlight_delimiter(index, state.options.delimiter);
        }
        cx.notify();
    }

    pub(super) fn close_csv_delimiter_menu(&mut self, cx: &mut Context<Self>) -> bool {
        let closed = self
            .csv_import
            .as_mut()
            .is_some_and(|state| state.delimiter_menu.take().is_some());
        if closed {
            cx.notify();
        }
        closed
    }

    fn csv_delimiter_key_down(
        &mut self,
        event: &gpui::KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = &mut self.csv_import else {
            return;
        };
        let key = &event.keystroke;
        if key.modifiers.platform || key.modifiers.control || key.modifiers.alt {
            return;
        }
        match (key.key.as_str(), state.delimiter_menu) {
            ("tab", _) => {
                state.sample_size.focus_handle(cx).focus(window);
                self.close_csv_delimiter_menu(cx);
            }
            ("space" | "down" | "up", None) => self.toggle_csv_delimiter_menu(window, cx),
            ("space", Some(byte)) => self.select_csv_delimiter(byte, cx),
            ("escape", Some(_)) => {
                self.close_csv_delimiter_menu(cx);
            }
            (_, Some(current)) => {
                let choices: Vec<_> = csv_delimiters().collect();
                let index = choices
                    .iter()
                    .position(|&byte| byte == current)
                    .unwrap_or(0);
                let last = choices.len() - 1;
                let next = match key.key.as_str() {
                    "up" => index.saturating_sub(1),
                    "down" => (index + 1).min(last),
                    "home" => 0,
                    "end" => last,
                    _ => {
                        let symbol = key.key_char.as_deref().unwrap_or(&key.key);
                        let [byte] = symbol.as_bytes() else {
                            return;
                        };
                        let Some(found) = choices.iter().position(|item| item == byte) else {
                            return;
                        };
                        found
                    }
                };
                state.highlight_delimiter(next, choices[next]);
                cx.notify();
            }
            _ => return,
        }
        cx.stop_propagation();
    }

    pub(super) fn render_csv_import(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(state) = &self.csv_import else {
            return div().into_any_element();
        };
        let c = Colors::new(self.preferences.dark);
        div()
            .min_w_0()
            .flex()
            .flex_col()
            .gap_2()
            .text_size(px(12.))
            .child(self.render_csv_settings_fields(state, cx))
            .child(self.render_csv_preview(state, state.preview_visible(cx), c, cx))
            .into_any_element()
    }

    pub(super) fn render_csv_import_heading(&self, cx: &mut Context<Self>) -> gpui::Div {
        let c = Colors::new(self.preferences.dark);
        let filename = self.csv_import.as_ref().map_or_else(String::new, |state| {
            state
                .path
                .file_name()
                .unwrap_or(state.path.as_os_str())
                .to_string_lossy()
                .into_owned()
        });
        div()
            .h(px(50.))
            .flex_shrink_0()
            .px(px(16.))
            .flex()
            .items_center()
            .gap(px(10.))
            .border_b_1()
            .border_color(rgb(c.grid))
            .child(icons::glyph(Icon::Table, 18., self.preferences.dark))
            .child(
                div()
                    .flex_shrink_0()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_size(px(15.))
                    .child(t("Text File Import")),
            )
            .child(div().w(px(1.)).h(px(14.)).flex_shrink_0().bg(rgb(c.border)))
            .child(
                div()
                    .id("csv-filename")
                    .min_w_0()
                    .flex_1()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_color(rgb(c.muted))
                    .text_size(px(12.))
                    .tooltip(icons::tooltip(filename.clone(), self.preferences.dark))
                    .child(filename),
            )
            .child(
                controls::icon_button("close-csv-import", c, true)
                    .tooltip(icons::tooltip(t("Close  esc"), self.preferences.dark))
                    .on_click(cx.listener(|this, _, window, cx| this.dismiss_panel(window, cx)))
                    .child(icons::glyph(Icon::Close, 14., self.preferences.dark)),
            )
    }

    fn render_csv_settings_fields(&self, state: &CsvImport, cx: &mut Context<Self>) -> gpui::Div {
        let c = Colors::new(self.preferences.dark);
        let checked = state.options.has_header;
        let valid = state.sample_records(cx).is_some();
        let header = controls::checkbox("csv-header", c, self.preferences.dark, checked, true)
            .child(t("Header row"))
            .on_click(cx.listener(|this, _, _, cx| {
                this.change_csv_structure(|options| options.has_header = !options.has_header, cx);
            }));
        let inference = div()
            .flex()
            .flex_shrink_0()
            .items_center()
            .gap_2()
            .text_color(rgb(if valid { c.muted } else { c.danger }))
            .child(t("Inference rows"))
            .child(self.input_shell(&state.sample_size, Some(68.)));
        div()
            .flex()
            .w_full()
            .items_center()
            .whitespace_nowrap()
            .justify_between()
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                if event.keystroke.key == "tab"
                    && let Some(state) = &this.csv_import
                {
                    let sample = state.sample_size.focus_handle(cx);
                    if state.delimiter_focus.is_focused(window) {
                        sample.focus(window);
                    } else {
                        state.delimiter_focus.focus(window);
                    }
                    this.close_csv_delimiter_menu(cx);
                    cx.stop_propagation();
                }
            }))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .max_w(px(232.))
                    .mr(px(16.))
                    .items_center()
                    .gap_2()
                    .child(div().text_color(rgb(c.muted)).child(t("Delimiter")))
                    .child(self.render_csv_delimiter_picker(state, c, cx)),
            )
            .child(header.mr(px(16.)))
            .child(inference.mr(px(16.)))
            .child(self.button(
                "import-preview",
                "Refresh Preview",
                valid && state.pending.is_none(),
                |this, _, cx| this.apply_csv_preview_options(cx),
                cx,
            ))
    }

    fn render_csv_delimiter_picker(
        &self,
        state: &CsvImport,
        c: Colors,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let open = state.delimiter_menu.is_some();
        let trigger = controls::selector("csv-delimiter", c, open, true)
            .track_focus(&state.delimiter_focus)
            .key_context("CsvDelimiter")
            .w_full()
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .truncate()
                    .child(csv_delimiter_label(state.options.delimiter)),
            )
            .child(controls::chevron(c, open))
            .on_key_down(cx.listener(Self::csv_delimiter_key_down))
            .on_action(cx.listener(|this, _: &text_input::Submit, window, cx| {
                if let Some(byte) = this
                    .csv_import
                    .as_ref()
                    .and_then(|state| state.delimiter_menu)
                {
                    this.select_csv_delimiter(byte, cx);
                } else {
                    this.toggle_csv_delimiter_menu(window, cx);
                }
            }))
            .on_click(
                cx.listener(|this, _, window, cx| this.toggle_csv_delimiter_menu(window, cx)),
            );
        div()
            .relative()
            .flex_1()
            .min_w(px(96.))
            .child(trigger)
            .when(open, |el| {
                el.child(
                    div()
                        .absolute()
                        .left_0()
                        .top(gpui::relative(1.))
                        .size(px(0.))
                        .child(gpui::deferred(
                            gpui::anchored()
                                .anchor(gpui::Corner::TopLeft)
                                .offset(point(px(0.), px(4.)))
                                .child(self.render_csv_delimiter_menu(state, c, cx)),
                        )),
                )
            })
    }

    fn render_csv_delimiter_menu(
        &self,
        state: &CsvImport,
        c: Colors,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let dark = self.preferences.dark;
        let selected = state.options.delimiter;
        let highlighted = state.delimiter_menu;
        let count = csv_delimiters().count();
        let rows = gpui::uniform_list(
            "csv-delimiter-options",
            count,
            cx.processor(move |_, range: std::ops::Range<usize>, _, cx| {
                csv_delimiters()
                    .enumerate()
                    .skip(range.start)
                    .take(range.len())
                    .map(|(index, byte)| {
                        controls::menu_item(
                            ("csv-delimiter-option", byte as usize),
                            c,
                            highlighted == Some(byte),
                            true,
                        )
                        .justify_between()
                        .when(index == COMMON_DELIMITERS.len(), |row| {
                            row.border_t_1().border_color(rgb(c.border))
                        })
                        .child(csv_delimiter_label(byte))
                        .when(byte == selected, |row| {
                            row.child(icons::glyph(Icon::Check, 13., dark))
                        })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.select_csv_delimiter(byte, cx);
                        }))
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .w_full()
        .h(px(controls::HEIGHT * 8.))
        .track_scroll(state.delimiter_scroll.clone());
        controls::menu_surface("csv-delimiter-menu", c)
            .occlude()
            .w(px(180.))
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|_, _, window, cx| {
                    // Let the trigger toggle itself and let other controls handle the click first.
                    let viewer = cx.weak_entity();
                    window.defer(cx, move |_, cx| {
                        let _ = viewer.update(cx, |this, cx| this.close_csv_delimiter_menu(cx));
                    });
                }),
            )
            .child(rows)
    }

    pub(super) fn render_csv_import_actions(&self, cx: &mut Context<Self>) -> gpui::Div {
        let c = Colors::new(self.preferences.dark);
        let state = self.csv_import.as_ref();
        let valid = state.is_some_and(|state| state.preview_current(cx));
        let picker = state
            .filter(|state| state.preview_visible(cx))
            .and_then(|state| state.type_column.map(|index| (state, index)));
        let mut row = div().flex().items_center().gap(px(12.)).min_h(px(30.));
        if let Some((state, index)) = picker {
            row = row.child(self.render_csv_type_choices(state, index, c, cx));
        } else {
            let hint = div()
                .flex_1()
                .min_w_0()
                .text_size(px(12.))
                .text_color(rgb(c.muted));
            row = row.child(hint.when(valid, |label| {
                label.child(t("Click a type below a column name to change it."))
            }));
        }
        div()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .gap(px(5.))
            .px(px(16.))
            .py(px(8.))
            .bg(rgb(c.bar))
            .border_t_1()
            .border_color(rgb(c.grid))
            .when(
                valid && state.is_some_and(|state| state.mode == ImportMode::ResetView),
                |el| {
                    el.child(div().text_size(px(12.)).text_color(rgb(c.warning)).child(t(
                        "The fields or filter changed. Importing will reset the table view.",
                    )))
                },
            )
            .child(row.child(self.button(
                "csv-import",
                "Import Text",
                valid,
                |this, _, cx| this.import_csv(cx),
                cx,
            )))
    }

    fn render_csv_preview(
        &self,
        state: &CsvImport,
        preview_visible: bool,
        c: Colors,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let preview = state.preview.as_ref().filter(|_| preview_visible);
        let count = preview.map_or(0, |probe| probe.columns.len());
        let start = state.first_column.min(count);
        let end = start.saturating_add(PREVIEW_COLUMNS).min(count);
        let error = state.error.as_ref().or(state.load_error.as_ref());
        let mut section = div()
            .w_full()
            .min_w_0()
            .flex()
            .flex_col()
            .gap(px(4.))
            .child(
                div()
                    .flex()
                    .h(px(24.))
                    .flex_shrink_0()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child(t("Data preview")),
                    )
                    .when_some(preview, |el, probe| {
                        el.child(
                            div().text_size(px(11.)).text_color(rgb(c.muted)).child(
                                t("Sample size · {count}")
                                    .replace("{count}", &probe.sampled_records.to_string()),
                            ),
                        )
                    })
                    .when(count > PREVIEW_COLUMNS, |el| {
                        let range = if start + 1 == end {
                            format!("{end} / {count}")
                        } else {
                            format!("{}–{end} / {count}", start + 1)
                        };
                        el.child(div().flex_1())
                            .child(
                                self.button(
                                    "csv-prev-columns",
                                    "‹",
                                    start > 0,
                                    |this, _, cx| this.csv_preview_page(false, cx),
                                    cx,
                                )
                                .size(px(23.))
                                .px_0()
                                .text_size(px(14.)),
                            )
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(rgb(c.muted))
                                    .child(range),
                            )
                            .child(
                                self.button(
                                    "csv-next-columns",
                                    "›",
                                    end < count,
                                    |this, _, cx| this.csv_preview_page(true, cx),
                                    cx,
                                )
                                .size(px(23.))
                                .px_0()
                                .text_size(px(14.)),
                            )
                    }),
            );
        let grid = div()
            .w_full()
            .min_w_0()
            .h(px(PREVIEW_GRID_HEIGHT))
            .flex_shrink_0()
            .flex()
            .border_1()
            .border_color(rgb(c.border))
            .bg(rgb(c.surface))
            .rounded(px(5.))
            .overflow_hidden();
        let Some(preview) = preview else {
            let (message, color) = match error {
                _ if state.sample_records(cx).is_none() => (inference_range_label(), c.danger),
                Some(error) => (t(error), c.danger),
                None if state.pending.is_some() => (t("Preparing text preview…"), c.muted),
                None => (t("Apply changes to update the preview."), c.muted),
            };
            return section.child(
                grid.id("csv-preview-status")
                    .flex_col()
                    .overflow_y_scroll()
                    .px_3()
                    .py_2()
                    .child(
                        div()
                            .whitespace_normal()
                            .text_color(rgb(color))
                            .child(message),
                    ),
            );
        };
        if let Some(error) = error {
            section = section.child(
                div()
                    .whitespace_normal()
                    .text_color(rgb(c.danger))
                    .child(t(error)),
            );
        }
        section.child(grid.children((start..end).map(|index| {
            let column = &preview.columns[index];
            let manual = state.options.column_types.contains_key(&index);
            let pointed = state.type_column == Some(index);
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .when(index > start, |el| {
                    el.border_l_1().border_color(rgb(c.grid))
                })
                .child(
                    div()
                        .min_w_0()
                        .px_2()
                        .h(px(PREVIEW_CELL_HEIGHT))
                        .flex()
                        .items_center()
                        .bg(rgb(if pointed { c.accent_soft } else { c.header }))
                        .text_color(rgb(if pointed { c.accent } else { c.text }))
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .child(column.name.clone()),
                        ),
                )
                .child(
                    div()
                        .id(("csv-column-type", index))
                        .h(px(PREVIEW_CELL_HEIGHT))
                        .px_2()
                        .min_w_0()
                        .flex()
                        .items_center()
                        .gap_1()
                        .cursor_pointer()
                        .text_size(px(11.))
                        .border_b_1()
                        .border_color(rgb(if pointed { c.accent } else { c.border }))
                        .bg(rgb(if pointed { c.accent_soft } else { c.header }))
                        .text_color(rgb(if manual || pointed { c.accent } else { c.muted }))
                        .hover(|el| el.bg(rgb(c.accent_soft)).text_color(rgb(c.accent)))
                        .tooltip(icons::tooltip(
                            t("Change column type"),
                            self.preferences.dark,
                        ))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .child(column.data_type.clone()),
                        )
                        .child(if pointed { "⌃" } else { "⌄" })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(state) = &mut this.csv_import {
                                state.type_column =
                                    (state.type_column != Some(index)).then_some(index);
                                cx.notify();
                            }
                        })),
                )
                .children(preview.rows.iter().take(PREVIEW_ROWS).enumerate().map(
                    |(row_index, row)| {
                        let value = row.get(index).map_or("", String::as_str);
                        let clipped = csv_preview_value(value);
                        div()
                            .h(px(PREVIEW_CELL_HEIGHT))
                            .min_w_0()
                            .px_2()
                            .flex()
                            .items_center()
                            .bg(rgb(if row_index.is_multiple_of(2) {
                                c.surface
                            } else {
                                c.stripe
                            }))
                            .child(div().min_w_0().truncate().child(clipped))
                    },
                ))
        })))
    }

    fn csv_preview_page(&mut self, forward: bool, cx: &mut Context<Self>) {
        if let Some(state) = &mut self.csv_import
            && let Some(preview) = &state.preview
        {
            let next = state.first_column.saturating_add(PREVIEW_COLUMNS);
            if forward && next < preview.columns.len() {
                state.first_column = next;
            } else if !forward {
                state.first_column = state.first_column.saturating_sub(PREVIEW_COLUMNS);
            }
            state.type_column = None;
            cx.notify();
        }
    }

    fn render_csv_type_choices(
        &self,
        state: &CsvImport,
        index: usize,
        c: Colors,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let selected = state.options.column_types.get(&index).copied();
        let name = state
            .preview
            .as_ref()
            .and_then(|probe| probe.columns.get(index))
            .map_or("", |column| column.name.as_str());
        let prompt = t("Choose a type for {column}").replace("{column}", &csv_preview_value(name));
        let options = [
            ("csv-type-auto", "Auto", "自动", None),
            ("csv-type-text", "Text", "文本", Some(CsvColumnType::Text)),
            (
                "csv-type-bool",
                "Boolean",
                "布尔",
                Some(CsvColumnType::Boolean),
            ),
            (
                "csv-type-int",
                "Integer",
                "整数",
                Some(CsvColumnType::Integer),
            ),
            (
                "csv-type-dec",
                "Decimal",
                "小数",
                Some(CsvColumnType::Decimal),
            ),
            (
                "csv-type-float",
                "Float",
                "浮点",
                Some(CsvColumnType::Float),
            ),
            ("csv-type-date", "Date", "日期", Some(CsvColumnType::Date)),
            (
                "csv-type-datetime",
                "DateTime",
                "日期时间",
                Some(CsvColumnType::DateTime),
            ),
        ];
        div()
            .flex_1()
            .min_w_0()
            .flex()
            .items_center()
            .gap(px(8.))
            .child(
                div()
                    .id("csv-type-target")
                    .max_w(px(180.))
                    .min_w_0()
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .text_size(px(12.))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .tooltip(icons::tooltip(prompt.clone(), self.preferences.dark))
                    .child(prompt),
            )
            .child(
                controls::segment_group(c)
                    .flex_shrink_0()
                    .children(options.into_iter().map(|(id, en, zh, kind)| {
                        let label = if self.preferences.language == Language::SimplifiedChinese {
                            zh
                        } else {
                            en
                        };
                        controls::segment(id, c, selected == kind)
                            .child(label)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.focus.focus(window);
                                this.choose_csv_type(index, kind, cx);
                            }))
                    })),
            )
    }
}

fn csv_preview_value(value: &str) -> String {
    let mut chars = value.chars();
    let mut display: String = chars
        .by_ref()
        .take(100)
        .map(|ch| match ch {
            '\r' | '\n' => '↵',
            '\t' => '⇥',
            _ => ch,
        })
        .collect();
    if chars.next().is_some() {
        display.push('…');
    }
    display
}
