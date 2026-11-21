//! Snapshot-owned Quick Peek content embedded in the dataset sidebar.
use crate::controls::{self, ButtonStyle};
use std::{path::PathBuf, sync::Arc, time::Duration};

use gpui::{
    App, Bounds, ClipboardItem, Context, Entity, EventEmitter, FocusHandle, Focusable, Image,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, RenderImage, ScrollHandle,
    UniformListScrollHandle, Window, canvas, div, img, point, prelude::*, px, rgb, size,
    uniform_list,
};

use crate::{
    app::{binary_extension, image_format, is_audio},
    audio::AudioPlayer,
    engine::Cell,
    i18n::t,
    icons::{Icon, glyph},
    nested::{JSON_PREVIEW_TRUNCATED, NestedTable, SqlType},
    text_input::{self, TextInput},
    theme::Colors,
};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct PeekKey {
    pub engine_id: usize,
    pub snapshot: String,
    pub row: usize,
    pub column: String,
    pub image_only: bool,
}

/// A cell can be requested again after hiding or switching the panel. The
/// request token prevents the earlier completion from republishing that cell.
pub(crate) fn pending_matches(
    expected_request: u64,
    current_request: u64,
    expected_key: &PeekKey,
    current_key: Option<&PeekKey>,
) -> bool {
    expected_request == current_request && current_key == Some(expected_key)
}

pub(crate) struct PeekData {
    pub prepared_nested: Option<NestedTable>,
    pub image: Option<Arc<RenderImage>>,
    pub copy_binary_format: Option<crate::formatting::BinaryFormat>,
    /// Image mode keeps only a shallow slice of this value for explicit raw
    /// copying. Opening an image never serializes its nested binary payload.
    pub copy_source: Option<crate::columnar::ArrowTable>,
    pub title: String,
    pub data_type: String,
    pub text: String,
    pub copy_text: String,
    pub cell: Cell,
    pub sources: Vec<PathBuf>,
    pub dark: bool,
    pub image_only: bool,
}

pub(super) fn binary_preview_text(value: &str, count: usize) -> String {
    let suffix = t("… ({count} bytes total; Save Value writes the original bytes)")
        .replace("{count}", &count.to_string());
    format!("{value}\n{suffix}")
}

impl PeekData {
    fn truncated_binary_text(&self) -> Option<String> {
        let count = self.cell.bytes.as_ref()?.len();
        (self.data_type == "BLOB" && !self.image_only && count > 1024)
            .then(|| binary_preview_text(&self.text, count))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PeekEvent {
    Close,
}

pub(crate) struct PeekView {
    focus: FocusHandle,
    detail: Entity<TextInput>,
    data: PeekData,
    audio: Option<AudioPlayer>,
    nested: Vec<NestedLevel>,
    nested_selection: Option<(usize, usize)>,
    nested_scroll_drag: Option<bool>,
    notice: String,
    notice_is_error: bool,
    seeking: bool,
}

struct NestedLevel {
    table: NestedTable,
    scroll: UniformListScrollHandle,
    horizontal: ScrollHandle,
}

/// Translate only the application's truncation suffix, never the JSON payload.
fn localized_json_preview(preview: &str) -> String {
    preview.strip_suffix(JSON_PREVIEW_TRUNCATED).map_or_else(
        || preview.to_owned(),
        |json| format!("{json}{}", t(JSON_PREVIEW_TRUNCATED)),
    )
}

/// App-generated title suffixes are separate from the actual dataset field.
/// Parse from the right so a field containing " · Row " stays verbatim.
pub(super) fn display_title(canonical: &str) -> String {
    let Some((column, suffix)) = canonical.rsplit_once(" · Row ") else {
        return canonical.to_owned();
    };
    let Some(row) = suffix.strip_suffix(" · Quick Peek") else {
        return canonical.to_owned();
    };
    if row.is_empty() || !row.bytes().all(|byte| byte.is_ascii_digit()) {
        return canonical.to_owned();
    }
    format!("{column} · {} {row} · {}", t("Row"), t("Quick Peek"))
}

impl EventEmitter<PeekEvent> for PeekView {}

impl PeekView {
    pub fn new(mut data: PeekData, cx: &mut Context<Self>) -> Self {
        let detail = cx.new(|cx| {
            TextInput::new(cx)
                .with_dark(data.dark)
                .with_multiline(true)
                .with_fill_height(true)
                .read_only(true)
        });
        let image_only = data.image_only && data.image.is_some();
        if data.image.is_some() {
            cx.on_release(|this, cx| {
                if let Some(image) = this.data.image.take() {
                    cx.drop_image(image, None);
                }
            })
            .detach();
        }
        let mut notice = String::new();
        let mut notice_is_error = false;
        let audio = data
            .cell
            .bytes
            .as_ref()
            .filter(|bytes| is_audio(bytes))
            .and_then(|bytes| match AudioPlayer::from_bytes(bytes.clone()) {
                Ok(player) => Some(player),
                Err(error) => {
                    notice = error.to_string();
                    notice_is_error = true;
                    None
                }
            });
        let nested = if data.image_only {
            Vec::new()
        } else {
            data.prepared_nested
                .take()
                .or_else(|| {
                    data.cell
                        .json
                        .take()
                        .or_else(|| {
                            (data.cell.is_null && SqlType::parse(&data.data_type).is_nested())
                                .then_some(serde_json::Value::Null)
                        })
                        .map(|value| NestedTable::new(value, &data.data_type, data.title.clone()))
                })
                .map(|table| {
                    vec![NestedLevel {
                        table,
                        scroll: UniformListScrollHandle::new(),
                        horizontal: ScrollHandle::new(),
                    }]
                })
                .unwrap_or_default()
        };
        if !image_only {
            let text = if nested.is_empty() {
                data.truncated_binary_text()
                    .unwrap_or_else(|| data.text.clone())
            } else {
                localized_json_preview(&data.text)
            };
            detail.update(cx, |input, cx| {
                input.set_text(&text, cx);
            });
        }
        cx.spawn(async move |weak, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(200))
                    .await;
                if weak
                    .update(cx, |this, cx| {
                        if this.audio.as_ref().is_some_and(|audio| audio.is_playing()) {
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        Self {
            focus: cx.focus_handle(),
            detail,
            data,
            audio,
            nested,
            nested_selection: None,
            nested_scroll_drag: None,
            notice,
            notice_is_error,
            seeking: false,
        }
    }

    pub fn activate(&self, window: &mut Window, cx: &mut Context<Self>) {
        window.activate_window();
        if self.data.image_only && self.data.image.is_some() {
            self.focus.focus(window);
        } else {
            self.detail.focus_handle(cx).focus(window);
        }
        cx.notify();
    }

    pub fn set_dark(&mut self, dark: bool, cx: &mut Context<Self>) {
        if self.data.dark != dark {
            self.data.dark = dark;
            self.detail.update(cx, |input, cx| input.set_dark(dark, cx));
            cx.notify();
        }
    }

    pub fn refresh_language(&mut self, cx: &mut Context<Self>) {
        let preview = self
            .nested
            .last()
            .map(|level| localized_json_preview(&level.table.node_json_preview()))
            .or_else(|| self.data.truncated_binary_text());
        if let Some(preview) = preview {
            self.detail
                .update(cx, |input, cx| input.set_text(&preview, cx));
        }
        cx.notify();
    }

    fn copy_value(&mut self, cx: &mut Context<Self>) {
        if let Some(source) = self.data.copy_source.clone() {
            let format = self.data.copy_binary_format;
            let task = cx
                .background_executor()
                .spawn(async move { source_copy_text(&source, format) });
            cx.spawn(async move |weak, cx| {
                let text = task.await;
                let _ = weak.update(cx, |this, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                    this.notice = "Copied to clipboard".into();
                    this.notice_is_error = false;
                    cx.notify();
                });
            })
            .detach();
            return;
        }
        cx.write_to_clipboard(ClipboardItem::new_string(
            if let Some(level) = self.nested.last() {
                level.table.node_json()
            } else if self.data.cell.is_null {
                String::new()
            } else {
                self.data.copy_text.clone()
            },
        ));
        self.notice = "Copied to clipboard".into();
        self.notice_is_error = false;
        cx.notify();
    }

    fn save_value(&mut self, cx: &mut Context<Self>) {
        let child_json = if self.nested.len() > 1
            || (!self.nested.is_empty() && self.data.cell.bytes.is_none())
        {
            self.nested.last().map(|level| level.table.node_json())
        } else {
            None
        };
        let extension = self
            .data
            .cell
            .bytes
            .as_ref()
            .filter(|_| child_json.is_none())
            .map(|bytes| binary_extension(bytes))
            .unwrap_or(if self.nested.is_empty() {
                "txt"
            } else {
                "json"
            });
        let directory = self
            .data
            .sources
            .first()
            .and_then(|path| path.parent())
            .unwrap_or(std::path::Path::new("/tmp"));
        let response = cx.prompt_for_new_path(directory, Some(&format!("cell.{extension}")));
        let bytes = child_json.map(String::into_bytes).unwrap_or_else(|| {
            self.data.cell.bytes.clone().unwrap_or_else(|| {
                if self.data.cell.text.is_empty() && !self.nested.is_empty() {
                    self.data.copy_text.as_bytes().to_vec()
                } else {
                    self.data.cell.text.as_bytes().to_vec()
                }
            })
        });
        let protected = self.data.sources.clone();
        cx.spawn(async move |weak, cx| {
            if let Ok(Ok(Some(path))) = response.await {
                let task = cx
                    .background_executor()
                    .spawn(async move { save_bytes(&path, &protected, &bytes) });
                let result = task.await;
                let _ = weak.update(cx, |this, cx| {
                    this.notice_is_error = result.is_err();
                    this.notice = match result {
                        Ok(()) => "Cell saved".into(),
                        Err(error) => error.to_string(),
                    };
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn button(
        &self,
        id: &'static str,
        label: &str,
        enabled: bool,
        action: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        cx: &Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let c = Colors::new(self.data.dark);
        let icon = if id == "peek-play" && label == "Pause" {
            Some(Icon::Pause)
        } else {
            Icon::for_command(id.trim_start_matches("peek-")).or(match id {
                "peek-save" => Some(Icon::Export),
                "peek-copy-table" => Some(Icon::Table),
                _ => None,
            })
        };
        controls::button(id, c, ButtonStyle::Standard, enabled)
            .on_click(cx.listener(move |this, _, window, cx| {
                cx.stop_propagation();
                if enabled {
                    action(this, window, cx);
                }
            }))
            .when_some(icon, |button, icon| {
                button.child(glyph(icon, 14., self.data.dark))
            })
            .tooltip(crate::icons::tooltip(t(label), self.data.dark))
            .child(t(label))
    }

    fn navigate_nested(&mut self, depth: usize, cx: &mut Context<Self>) {
        self.nested.truncate(depth + 1);
        self.nested_selection = None;
        self.nested_scroll_drag = None;
        if let Some(level) = self.nested.last() {
            let text = localized_json_preview(&level.table.node_json_preview());
            self.detail
                .update(cx, |input, cx| input.set_text(&text, cx));
        }
        cx.notify();
    }

    fn select_nested_cell(
        &mut self,
        row: usize,
        column: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(level) = self.nested.last() else {
            return;
        };
        let child = level.table.open_cell(row, column);
        if let Some(child) = child {
            let preview = localized_json_preview(&child.node_json_preview());
            self.nested.push(NestedLevel {
                table: child,
                scroll: UniformListScrollHandle::new(),
                horizontal: ScrollHandle::new(),
            });
            self.nested_selection = None;
            self.nested_scroll_drag = None;
            self.detail
                .update(cx, |input, cx| input.set_text(&preview, cx));
        } else {
            self.nested_selection = Some((row, column));
        }
        self.focus.focus(window);
        cx.notify();
    }

    fn render_nested_row(&self, row: usize, cx: &Context<Self>) -> gpui::Div {
        let Some(level) = self.nested.last() else {
            return div();
        };
        let c = Colors::new(self.data.dark);
        div()
            .flex()
            .h(px(28.))
            .w(px(76. + level.table.columns.len() as f32 * 200.))
            .flex_shrink_0()
            .border_b_1()
            .border_color(rgb(c.grid))
            .bg(rgb(if row.is_multiple_of(2) {
                c.surface
            } else {
                c.stripe
            }))
            .hover(|row| row.bg(rgb(c.hover)))
            .child(
                div()
                    .w(px(76.))
                    .h_full()
                    .flex_shrink_0()
                    .px_2()
                    .py_1()
                    .border_r_1()
                    .border_color(rgb(c.grid))
                    .text_color(rgb(c.muted))
                    .truncate()
                    .child(level.table.row_label(row)),
            )
            .children((0..level.table.columns.len()).map(|column| {
                let selected = self.nested_selection == Some((row, column));
                let expandable = level.table.is_expandable(row, column);
                let missing = level.table.cell_missing_kind(row, column).is_some();
                div()
                    .id(("peek-nested-cell", row * level.table.columns.len() + column))
                    .w(px(200.))
                    .h_full()
                    .flex_shrink_0()
                    .px_2()
                    .py_1()
                    .border_r_1()
                    .border_color(rgb(c.grid))
                    .truncate()
                    .text_color(rgb(c.value_foreground(missing, expandable)))
                    .when(selected, |cell| cell.bg(rgb(c.selected)))
                    .when(expandable, |cell| cell.cursor_pointer())
                    .child(level.table.cell_preview(row, column).unwrap_or_default())
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.select_nested_cell(row, column, window, cx);
                    }))
            }))
    }

    fn nested_scrollbar(&self, horizontal: bool, cx: &Context<Self>) -> gpui::AnyElement {
        let weak = cx.entity().downgrade();
        let track = canvas(
            |_, _, _| (),
            move |bounds, _, window, cx| {
                let Some(view) = weak.upgrade() else {
                    return;
                };
                let this = view.read(cx);
                let c = Colors::new(this.data.dark);
                let Some(level) = this.nested.last() else {
                    return;
                };
                let (handle, total) = if horizontal {
                    (
                        level.horizontal.clone(),
                        px(76. + level.table.columns.len() as f32 * 200.),
                    )
                } else {
                    (
                        level.scroll.0.borrow().base_handle.clone(),
                        px(level.table.row_count() as f32 * 28.),
                    )
                };
                let viewport = if horizontal {
                    bounds.size.width
                } else {
                    bounds.size.height
                };
                let maximum = (total - viewport).max(px(0.));
                if maximum <= px(0.) || viewport <= px(0.) {
                    return;
                }
                let current = if horizontal {
                    -handle.offset().x
                } else {
                    -handle.offset().y
                };
                let thumb = (viewport * (viewport / total)).max(px(24.)).min(viewport);
                let travel = (viewport - thumb).max(px(1.));
                let top = travel * (current / maximum).clamp(0., 1.);
                window.paint_quad(gpui::fill(bounds, rgb(c.bar)));
                let thumb_bounds = if horizontal {
                    Bounds::new(
                        point(bounds.left() + top, bounds.top() + px(2.)),
                        size(thumb, (bounds.size.height - px(4.)).max(px(1.))),
                    )
                } else {
                    Bounds::new(
                        point(bounds.left() + px(2.), bounds.top() + top),
                        size((bounds.size.width - px(4.)).max(px(1.)), thumb),
                    )
                };
                window.paint_quad(gpui::fill(thumb_bounds, rgb(c.scrollbar)));
                let target = weak.clone();
                let scroll = handle.clone();
                window.on_mouse_event(move |event: &MouseDownEvent, phase, _, cx| {
                    if phase != gpui::DispatchPhase::Bubble
                        || event.button != MouseButton::Left
                        || !bounds.contains(&event.position)
                    {
                        return;
                    }
                    let at = if horizontal {
                        event.position.x - bounds.left()
                    } else {
                        event.position.y - bounds.top()
                    };
                    let position = ((at - thumb / 2.) / travel).clamp(0., 1.) * maximum;
                    let old = scroll.offset();
                    scroll.set_offset(if horizontal {
                        point(-position, old.y)
                    } else {
                        point(old.x, -position)
                    });
                    if let Some(view) = target.upgrade() {
                        view.update(cx, |this, cx| {
                            this.nested_scroll_drag = Some(horizontal);
                            cx.notify();
                        });
                    }
                    cx.stop_propagation();
                });
                let target = weak.clone();
                let scroll = handle.clone();
                window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, cx| {
                    if phase != gpui::DispatchPhase::Bubble || !event.dragging() {
                        return;
                    }
                    let Some(view) = target
                        .upgrade()
                        .filter(|view| view.read(cx).nested_scroll_drag == Some(horizontal))
                    else {
                        return;
                    };
                    let at = if horizontal {
                        event.position.x - bounds.left()
                    } else {
                        event.position.y - bounds.top()
                    };
                    let position = ((at - thumb / 2.) / travel).clamp(0., 1.) * maximum;
                    let old = scroll.offset();
                    scroll.set_offset(if horizontal {
                        point(-position, old.y)
                    } else {
                        point(old.x, -position)
                    });
                    view.update(cx, |_, cx| cx.notify());
                    cx.stop_propagation();
                });
                let target = weak.clone();
                window.on_mouse_event(move |_: &MouseUpEvent, phase, _, cx| {
                    if phase == gpui::DispatchPhase::Capture {
                        if let Some(view) = target.upgrade() {
                            view.update(cx, |this, _| this.nested_scroll_drag = None);
                        }
                    }
                });
            },
        );
        if horizontal {
            track
                .absolute()
                .left_0()
                .right(px(12.))
                .bottom_0()
                .h(px(12.))
                .into_any_element()
        } else {
            track
                .absolute()
                .right_0()
                .top(px(40.))
                .bottom(px(12.))
                .w(px(12.))
                .into_any_element()
        }
    }

    fn nested_view(&self, cx: &Context<Self>) -> gpui::AnyElement {
        let Some(level) = self.nested.last() else {
            return div().into_any_element();
        };
        let c = Colors::new(self.data.dark);
        let width = 76. + level.table.columns.len() as f32 * 200.;
        let row_count = level.table.row_count();
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(self.button(
                        "peek-nested-back",
                        "‹ Back",
                        self.nested.len() > 1,
                        |this, _, cx| {
                            this.navigate_nested(this.nested.len().saturating_sub(2), cx);
                        },
                        cx,
                    ))
                    .child(
                        div()
                            .id("peek-breadcrumbs")
                            .flex_1()
                            .min_w_0()
                            .overflow_x_scroll()
                            .flex()
                            .gap_1()
                            .children(self.nested.iter().enumerate().map(|(depth, level)| {
                                div()
                                    .id(("peek-breadcrumb", depth))
                                    .cursor_pointer()
                                    .whitespace_nowrap()
                                    .px_2()
                                    .py_1()
                                    .rounded(px(3.))
                                    .text_color(rgb(if depth + 1 == self.nested.len() {
                                        c.text
                                    } else {
                                        c.muted
                                    }))
                                    .hover(|item| item.bg(rgb(c.hover)).text_color(rgb(c.accent)))
                                    .child(format!(
                                        "{}{}",
                                        if depth == 0 { "" } else { "› " },
                                        if depth == 0 {
                                            display_title(&level.table.label)
                                        } else {
                                            level.table.label.clone()
                                        }
                                    ))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.navigate_nested(depth, cx)
                                    }))
                            })),
                    )
                    .child(self.button(
                        "peek-copy-table",
                        "Copy Table",
                        true,
                        |this, _, cx| {
                            if let Some(level) = this.nested.last() {
                                match level.table.to_tsv() {
                                    Ok(text) => {
                                        cx.write_to_clipboard(ClipboardItem::new_string(text));
                                        this.notice = "Table copied as TSV".into();
                                        this.notice_is_error = false;
                                    }
                                    Err(error) => {
                                        this.notice = error.to_string();
                                        this.notice_is_error = true;
                                    }
                                }
                                cx.notify();
                            }
                        },
                        cx,
                    )),
            )
            .child(
                div()
                    .text_size(px(11.))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(rgb(c.muted))
                    .child(t(level.table.summary())),
            )
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .pb(px(12.))
                    .border_1()
                    .border_color(rgb(c.border))
                    .rounded(px(3.))
                    .overflow_hidden()
                    .bg(rgb(c.surface))
                    .child(
                        div()
                            .id("peek-nested-table-viewport")
                            .size_full()
                            .overflow_x_scroll()
                            .track_scroll(&level.horizontal)
                            .child(
                                div()
                                    .w(px(width))
                                    .h_full()
                                    .flex()
                                    .flex_col()
                                    .child(
                                        div()
                                            .h(px(40.))
                                            .flex_shrink_0()
                                            .flex()
                                            .border_b_1()
                                            .border_color(rgb(c.border))
                                            .bg(rgb(c.header))
                                            .child(
                                                div()
                                                    .w(px(76.))
                                                    .h_full()
                                                    .flex_shrink_0()
                                                    .px_2()
                                                    .py_1()
                                                    .border_r_1()
                                                    .border_color(rgb(c.grid))
                                                    .text_color(rgb(c.muted))
                                                    .child("#"),
                                            )
                                            .children(level.table.columns.iter().map(|column| {
                                                div()
                                                    .w(px(200.))
                                                    .h_full()
                                                    .flex_shrink_0()
                                                    .px_2()
                                                    .border_r_1()
                                                    .border_color(rgb(c.grid))
                                                    .overflow_hidden()
                                                    .child(
                                                        div()
                                                            .font_weight(gpui::FontWeight::MEDIUM)
                                                            .truncate()
                                                            .child(column.name.clone()),
                                                    )
                                                    .child(
                                                        div()
                                                            .text_size(px(10.))
                                                            .text_color(rgb(c.muted))
                                                            .truncate()
                                                            .child(column.data_type.clone()),
                                                    )
                                            })),
                                    )
                                    .when(row_count == 0, |table| {
                                        table.child(
                                            div()
                                                .p_3()
                                                .text_color(rgb(c.muted))
                                                .child(t("Empty collection — 0 rows")),
                                        )
                                    })
                                    .when(row_count > 0, |table| {
                                        table.child(
                                            uniform_list(
                                                "peek-nested-rows",
                                                row_count,
                                                cx.processor(
                                                    |this, rows: std::ops::Range<usize>, _, cx| {
                                                        rows.map(|row| {
                                                            this.render_nested_row(row, cx)
                                                        })
                                                        .collect()
                                                    },
                                                ),
                                            )
                                            .w(px(width))
                                            .flex_1()
                                            .min_h_0()
                                            .track_scroll(level.scroll.clone()),
                                        )
                                    }),
                            ),
                    )
                    .child(self.nested_scrollbar(false, cx))
                    .child(self.nested_scrollbar(true, cx)),
            )
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(rgb(c.muted))
                    .child(t(level.table.representation_note())),
            )
            .into_any_element()
    }

    fn audio_view(&self, cx: &Context<Self>) -> gpui::AnyElement {
        let Some(audio) = &self.audio else {
            return div().into_any_element();
        };
        let position = audio.position();
        let duration = audio.duration();
        let weak = cx.entity().downgrade();
        let c = Colors::new(self.data.dark);
        div()
            .flex()
            .flex_col()
            .gap_3()
            .py_2()
            .border_t_1()
            .border_b_1()
            .border_color(rgb(c.border))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(self.button(
                        "peek-play",
                        if audio.is_playing() { "Pause" } else { "Play" },
                        true,
                        |this, _, cx| {
                            if let Some(audio) = &this.audio {
                                if audio.is_playing() {
                                    audio.pause();
                                } else if let Err(error) = audio.play() {
                                    this.notice = error.to_string();
                                    this.notice_is_error = true;
                                }
                            }
                            cx.notify();
                        },
                        cx,
                    ))
                    .child(self.button(
                        "peek-stop",
                        "Stop",
                        true,
                        |this, _, cx| {
                            if let Some(audio) = &this.audio {
                                audio.stop();
                            }
                            cx.notify();
                        },
                        cx,
                    ))
                    .child(
                        div()
                            .text_color(rgb(c.muted))
                            .child(t(format!("{position:.1} / {duration:.1} s"))),
                    ),
            )
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(rgb(c.muted))
                    .child(t(audio.format_description())),
            )
            .child(
                canvas(
                    |_, _, _| (),
                    move |bounds, _, window, _| {
                        window.paint_quad(gpui::fill(bounds, rgb(c.accent_soft)));
                        if duration <= 0. || bounds.size.width <= px(0.) {
                            return;
                        }
                        window.paint_quad(gpui::fill(
                            Bounds::new(
                                bounds.origin,
                                size(
                                    bounds.size.width * (position / duration) as f32,
                                    bounds.size.height,
                                ),
                            ),
                            rgb(c.primary),
                        ));
                        let target = weak.clone();
                        window.on_mouse_event(move |event: &MouseDownEvent, phase, _, cx| {
                            if phase != gpui::DispatchPhase::Bubble
                                || event.button != MouseButton::Left
                                || !bounds.contains(&event.position)
                            {
                                return;
                            }
                            if let Some(view) = target.upgrade() {
                                view.update(cx, |this, cx| {
                                    if let Some(audio) = &this.audio {
                                        audio.seek(
                                            f64::from(
                                                ((event.position.x - bounds.left())
                                                    / bounds.size.width)
                                                    .clamp(0., 1.),
                                            ) * duration,
                                        );
                                    }
                                    this.seeking = true;
                                    cx.notify();
                                });
                            }
                        });
                        let target = weak.clone();
                        window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, cx| {
                            if phase != gpui::DispatchPhase::Bubble || !event.dragging() {
                                return;
                            }
                            if let Some(view) =
                                target.upgrade().filter(|view| view.read(cx).seeking)
                            {
                                view.update(cx, |this, cx| {
                                    if let Some(audio) = &this.audio {
                                        audio.seek(
                                            f64::from(
                                                ((event.position.x - bounds.left())
                                                    / bounds.size.width)
                                                    .clamp(0., 1.),
                                            ) * duration,
                                        );
                                    }
                                    cx.notify();
                                });
                            }
                        });
                        let target = weak.clone();
                        window.on_mouse_event(move |_: &MouseUpEvent, phase, _, cx| {
                            if phase == gpui::DispatchPhase::Capture {
                                if let Some(view) = target.upgrade() {
                                    view.update(cx, |this, _| this.seeking = false);
                                }
                            }
                        });
                    },
                )
                .w_full()
                .h(px(10.)),
            )
            .into_any_element()
    }
}

impl Focusable for PeekView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for PeekView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = Colors::new(self.data.dark);
        let title = display_title(&self.data.title);
        let image_only = self.data.image_only && self.data.image.is_some();
        let content = div()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .p_3()
            .flex()
            .flex_col()
            .gap_3()
            .overflow_hidden()
            .child(
                div()
                    .w_full()
                    .min_w_0()
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .child(
                        div()
                            .id("preview-data-type")
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(11.))
                            .text_color(rgb(c.muted))
                            .tooltip(crate::icons::tooltip(
                                self.data.data_type.clone(),
                                self.data.dark,
                            ))
                            .child(self.data.data_type.clone()),
                    ),
            )
            .when(image_only, |element| {
                let bytes = self.data.cell.bytes.as_deref().unwrap_or_default();
                element.child(
                    div()
                        .flex_shrink_0()
                        .text_size(px(11.))
                        .text_color(rgb(c.muted))
                        .child(t(format!(
                            "{} image · {} bytes",
                            binary_extension(bytes).to_ascii_uppercase(),
                            bytes.len()
                        ))),
                )
            })
            .when_some(self.data.image.clone(), |element, image| {
                let image = if image_only {
                    div()
                        .relative()
                        .size_full()
                        .min_h_0()
                        .min_w_0()
                        .child(
                            img(image)
                                // Both relative dimensions keep intrinsic size
                                // out of the flex pane's height calculation.
                                .absolute()
                                .inset_0()
                                .size_full()
                                .object_fit(gpui::ObjectFit::Contain),
                        )
                        .into_any_element()
                } else {
                    img(image)
                        .h(px(200.))
                        .max_w_full()
                        .object_fit(gpui::ObjectFit::Contain)
                        .into_any_element()
                };
                element.child(
                    div()
                        .flex()
                        .items_center()
                        .justify_center()
                        .p_2()
                        .rounded(px(3.))
                        .bg(rgb(c.surface))
                        .when(image_only, |frame| {
                            frame.flex_1().min_h_0().min_w_0().overflow_hidden()
                        })
                        .when(!image_only, |frame| frame.flex_shrink_0())
                        .child(image),
                )
            })
            .when(!image_only && self.audio.is_some(), |element| {
                element.child(self.audio_view(cx))
            })
            .when(!image_only, |element| {
                element.child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .overflow_hidden()
                        .when(!self.nested.is_empty(), |element| {
                            element.child(
                                div()
                                    .flex_basis(px(0.))
                                    .map(|mut table| {
                                        table.style().flex_grow = Some(55.);
                                        table
                                    })
                                    .min_h_0()
                                    .flex()
                                    .flex_col()
                                    .overflow_hidden()
                                    .child(self.nested_view(cx)),
                            )
                        })
                        .child(
                            div()
                                .flex_basis(px(0.))
                                .map(|mut detail| {
                                    detail.style().flex_grow = Some(45.);
                                    detail
                                })
                                .min_h_0()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .overflow_hidden()
                                .when(!self.nested.is_empty(), |element| {
                                    element.child(
                                        div()
                                            .flex_shrink_0()
                                            .text_size(px(11.))
                                            .font_weight(gpui::FontWeight::MEDIUM)
                                            .text_color(rgb(c.muted))
                                            .child(t("JSON")),
                                    )
                                })
                                .child(
                                    div()
                                        .flex_1()
                                        .min_h_0()
                                        .overflow_hidden()
                                        .child(self.detail.clone()),
                                ),
                        ),
                )
            })
            .child(
                div()
                    .flex()
                    .flex_shrink_0()
                    .flex_wrap()
                    .gap_2()
                    .pt_2()
                    .border_t_1()
                    .border_color(rgb(c.border))
                    .child(self.button(
                        "peek-copy",
                        if self.nested.is_empty() {
                            "Copy Value"
                        } else {
                            "Copy Node"
                        },
                        true,
                        |this, _, cx| this.copy_value(cx),
                        cx,
                    ))
                    .when(!self.nested.is_empty(), |element| {
                        element.child(self.button(
                            "peek-copy-cell",
                            "Copy Cell",
                            self.nested_selection.is_some(),
                            |this, _, cx| {
                                if let Some((row, column)) = this.nested_selection {
                                    if let Some(text) = this
                                        .nested
                                        .last()
                                        .and_then(|level| level.table.cell_text(row, column))
                                    {
                                        cx.write_to_clipboard(ClipboardItem::new_string(text));
                                        this.notice = "Copied to clipboard".into();
                                        this.notice_is_error = false;
                                        cx.notify();
                                    }
                                }
                            },
                            cx,
                        ))
                    })
                    .child(self.button(
                        "peek-save",
                        "Save Value…",
                        true,
                        |this, _, cx| this.save_value(cx),
                        cx,
                    ))
                    .when(self.data.image.is_some(), |element| {
                        element.child(self.button(
                            "peek-copy-image",
                            "Copy Image",
                            true,
                            |this, _, cx| {
                                if let Some(bytes) = &this.data.cell.bytes
                                    && let Some(format) = image_format(bytes)
                                {
                                    cx.write_to_clipboard(ClipboardItem::from(Image::from_bytes(
                                        format,
                                        bytes.clone(),
                                    )));
                                }
                            },
                            cx,
                        ))
                    }),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .text_size(px(11.))
                    .text_color(rgb(if self.notice_is_error {
                        c.danger
                    } else {
                        c.success
                    }))
                    .when(!self.notice.is_empty(), |notice| {
                        notice
                            .px_2()
                            .py_1()
                            .rounded(px(3.))
                            .bg(rgb(if self.notice_is_error {
                                c.danger_bg
                            } else {
                                c.success_bg
                            }))
                    })
                    .truncate()
                    .child(t(&self.notice)),
            );
        div()
            .size_full()
            .flex()
            .flex_col()
            .track_focus(&self.focus)
            .font_family("Helvetica Neue")
            .text_size(px(12.))
            .bg(rgb(c.bar))
            .text_color(rgb(c.text))
            .on_action(cx.listener(|_, _: &crate::app::CloseWindow, _, cx| {
                cx.emit(PeekEvent::Close);
                cx.stop_propagation();
            }))
            .on_action(cx.listener(|_, _: &crate::app::Dismiss, _, cx| {
                cx.emit(PeekEvent::Close);
                cx.stop_propagation();
            }))
            .on_action(cx.listener(|this, _: &crate::app::Copy, window, cx| {
                if !(this.data.image_only && this.data.image.is_some())
                    && this.detail.focus_handle(cx).is_focused(window)
                {
                    window.dispatch_action(Box::new(text_input::Copy), cx);
                } else {
                    this.copy_value(cx);
                }
                cx.stop_propagation();
            }))
            .on_action(cx.listener(|this, _: &crate::app::CopyHeaders, _, cx| {
                if let Some(level) = this.nested.last() {
                    match level.table.to_tsv() {
                        Ok(text) => {
                            cx.write_to_clipboard(ClipboardItem::new_string(text));
                            this.notice = "Table copied as TSV".into();
                            this.notice_is_error = false;
                        }
                        Err(error) => {
                            this.notice = error.to_string();
                            this.notice_is_error = true;
                        }
                    }
                    cx.notify();
                } else {
                    this.copy_value(cx);
                }
                cx.stop_propagation();
            }))
            .on_action(cx.listener(|this, _: &crate::app::SelectAll, window, cx| {
                if !(this.data.image_only && this.data.image.is_some()) {
                    this.detail.focus_handle(cx).focus(window);
                    window.dispatch_action(Box::new(text_input::SelectAll), cx);
                }
                cx.stop_propagation();
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .p_3()
                    .flex_shrink_0()
                    .min_w_0()
                    .border_b_1()
                    .border_color(rgb(c.border))
                    .child(glyph(
                        if image_only { Icon::Image } else { Icon::Table },
                        16.,
                        self.data.dark,
                    ))
                    .child(
                        div()
                            .id("preview-title")
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .tooltip(crate::icons::tooltip(title.clone(), self.data.dark))
                            .child(title),
                    )
                    .child(
                        controls::icon_button("peek-close", c, true)
                            .child(glyph(Icon::Close, 14., self.data.dark))
                            .tooltip(crate::icons::tooltip(t("Close"), self.data.dark))
                            .on_click(cx.listener(|_, _, _, cx| {
                                cx.stop_propagation();
                                cx.emit(PeekEvent::Close);
                            })),
                    ),
            )
            .child(content)
    }
}

fn source_copy_text(
    source: &crate::columnar::ArrowTable,
    format: Option<crate::formatting::BinaryFormat>,
) -> String {
    if let Some(text) =
        format.and_then(|format| source.binary(0, 0).and_then(|bytes| format.apply(bytes)))
    {
        return text;
    }
    source.cell(0, 0).text
}

fn save_bytes(path: &std::path::Path, protected: &[PathBuf], bytes: &[u8]) -> std::io::Result<()> {
    crate::source_guard::save_copy(path, protected, bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn localized_peek_chrome_preserves_field_names_and_json_payloads() {
        for column in ["Open", "Row", "中文 · Row 7", "photo · Quick Peek"] {
            let title = display_title(&format!("{column} · Row 123 · Quick Peek"));
            assert!(title.starts_with(&format!("{column} · ")));
            assert!(title.ends_with(&t("Quick Peek")));
        }
        for value in ["Open", "photo · Row nope · Quick Peek", "photo · Row 1"] {
            assert_eq!(display_title(value), value);
        }
        let json = "{\"Open\":\"Close\",\"Row\":\"Quick Peek\",\"中文\":\"Table copied as TSV\"}";
        assert_eq!(localized_json_preview(json), json);
        let truncated = localized_json_preview(&format!("{json}{JSON_PREVIEW_TRUNCATED}"));
        assert_eq!(&truncated[..json.len()], json);
        assert_eq!(&truncated[json.len()..], t(JSON_PREVIEW_TRUNCATED));
    }

    #[test]
    fn deferred_image_copy_preserves_full_binary_format_and_photo_json() -> anyhow::Result<()> {
        use crate::{columnar::ArrowTable, engine::ColumnInfo, formatting::BinaryFormat};
        for bytes in [vec![0, 65, 255], vec![7; 16], vec![65; 4096]] {
            let source = ArrowTable::from_cells(
                vec![ColumnInfo {
                    name: "image".into(),
                    data_type: "BLOB".into(),
                    nullable: true,
                }],
                vec![vec![Cell {
                    bytes: Some(bytes.clone()),
                    ..Default::default()
                }]],
            );
            for &format in BinaryFormat::all() {
                assert_eq!(
                    source_copy_text(&source, Some(format)),
                    format
                        .apply(&bytes)
                        .unwrap_or_else(|| source.cell(0, 0).text),
                    "{format:?}, {} bytes",
                    bytes.len(),
                );
            }
            assert_eq!(
                source_copy_text(&source, Some(BinaryFormat::Hex)).len(),
                bytes.len() * 3 - 1
            );
        }
        let source = ArrowTable::from_cells(
            vec![ColumnInfo {
                name: "photo".into(),
                data_type: "STRUCT(bytes BLOB, path VARCHAR)".into(),
                nullable: true,
            }],
            vec![vec![Cell {
                json: Some(serde_json::json!({"bytes":[137,80],"path":"中文.png"})),
                ..Default::default()
            }]],
        );
        let copy = source_copy_text(&source, None);
        let json: serde_json::Value = serde_json::from_str(&copy)?;
        assert_eq!(json["path"], "中文.png");
        assert!(
            json["bytes"]
                .as_str()
                .is_some_and(|bytes| !bytes.is_empty())
        );
        Ok(())
    }

    #[test]
    fn preview_identity_and_request_token_reject_obsolete_completions() {
        let nested = PeekKey {
            engine_id: 42,
            snapshot: "result_1".into(),
            row: 7,
            column: "photo".into(),
            image_only: false,
        };
        let image = PeekKey {
            image_only: true,
            ..nested.clone()
        };
        assert_ne!(nested, image);
        assert!(pending_matches(11, 11, &nested, Some(&nested)));
        assert!(!pending_matches(11, 11, &nested, Some(&image)));
        // A newer request can target exactly the same cell after the sidebar
        // closes. Matching its cell key alone must not revive the old work.
        assert!(!pending_matches(11, 12, &nested, Some(&nested)));
        assert!(!pending_matches(11, 12, &nested, None));
        for other in [
            PeekKey {
                engine_id: 43,
                ..nested.clone()
            },
            PeekKey {
                snapshot: "result_2".into(),
                ..nested.clone()
            },
            PeekKey {
                row: 8,
                ..nested.clone()
            },
            PeekKey {
                column: "other".into(),
                ..nested.clone()
            },
        ] {
            assert!(!pending_matches(11, 11, &nested, Some(&other)));
        }
    }

    #[test]
    fn save_keeps_original_bytes_and_rejects_source_aliases() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let source = directory.path().join("source.parquet");
        std::fs::write(&source, b"source")?;
        let alias = directory.path().join("alias.parquet");
        std::fs::hard_link(&source, &alias)?;
        assert!(save_bytes(&alias, &[source.clone()], b"corrupt").is_err());
        assert_eq!(std::fs::read(&source)?, b"source");
        let target = directory.path().join("preview.bin");
        save_bytes(&target, &[source], b"\0\xffexact")?;
        assert_eq!(std::fs::read(target)?, b"\0\xffexact");
        Ok(())
    }
}
