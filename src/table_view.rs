//! Four synchronized virtual panes. Display coordinates never change the data
//! snapshot: pinned rows/columns are mapped back to their original indices.
use super::*;
#[cfg(test)]
use crate::engine::Cell;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CellPreview {
    Image,
    Nested,
}

/// Classify only when a preview is requested, not on every frame. Real image
/// bytes take precedence over a photo STRUCT's JSON; filenames and JSON-encoded
/// byte strings alone never cause file reads or pretend to be an image.
fn preview_kind(missing: bool, bytes: Option<&[u8]>, nested: bool) -> Option<CellPreview> {
    if missing {
        None
    } else if bytes.is_some_and(|bytes| image_format(bytes).is_some()) {
        Some(CellPreview::Image)
    } else if nested {
        Some(CellPreview::Nested)
    } else {
        None
    }
}

#[cfg(test)]
fn double_click_preview(cell: &Cell) -> Option<CellPreview> {
    preview_kind(
        cell.is_null,
        cell.bytes.as_deref(),
        cell.json
            .as_ref()
            .is_some_and(|value| value.is_object() || value.is_array()),
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SelectionRegion {
    top: usize,
    bottom: usize,
    left: usize,
    right: usize,
}

/// Normalize the drag direction and intersect the selection with the table.
fn selection_region(
    selection: Option<((usize, usize), (usize, usize))>,
    rows: std::ops::Range<usize>,
    columns: std::ops::Range<usize>,
) -> Option<SelectionRegion> {
    let (anchor, end) = selection?;
    if rows.is_empty() || columns.is_empty() {
        return None;
    }
    let region = SelectionRegion {
        top: anchor.0.min(end.0).max(rows.start),
        bottom: anchor.0.max(end.0).min(rows.end - 1),
        left: anchor.1.min(end.1).max(columns.start),
        right: anchor.1.max(end.1).min(columns.end - 1),
    };
    (region.top <= region.bottom && region.left <= region.right).then_some(region)
}

const COLUMN_OVERSCAN: f32 = 120.;
const SELECTION_STROKE: f32 = 2.;

/// One prefix sum for the table, shared by all visible rows and both column
/// panes. Only this small geometry array spans the full column count.
#[derive(Clone)]
struct ColumnLayout(std::sync::Arc<Vec<f32>>);

impl ColumnLayout {
    fn new(order: &[usize], widths: &[f32]) -> Self {
        let mut offsets = Vec::with_capacity(order.len() + 1);
        offsets.push(0.);
        for &source in order {
            let width = widths.get(source).copied().unwrap_or(150.);
            let width = if width.is_finite() && width > 0. {
                width
            } else {
                150.
            };
            offsets.push(offsets.last().copied().unwrap_or(0.) + width);
        }
        Self(std::sync::Arc::new(offsets))
    }

    fn width(&self, columns: std::ops::Range<usize>) -> f32 {
        self.0[columns.end] - self.0[columns.start]
    }

    fn viewport(
        &self,
        pane: std::ops::Range<usize>,
        number: bool,
        scroll: f32,
        viewport_width: f32,
    ) -> ColumnViewport {
        let gutter = if number { 52. } else { 0. };
        let total_width = gutter + self.width(pane.clone());
        let viewport_width = viewport_width.max(0.);
        let scroll = if scroll.is_finite() {
            scroll.clamp(0., (total_width - viewport_width).max(0.))
        } else {
            0.
        };
        let origin = self.0[pane.start];
        let left = origin + scroll - gutter - COLUMN_OVERSCAN;
        let right = origin + scroll + viewport_width - gutter + COLUMN_OVERSCAN;
        // A column whose right edge equals the lower boundary has no pixels
        // in the buffered viewport. Symmetric strictness excludes the first
        // column whose left edge equals the upper boundary.
        let first =
            pane.start + self.0[pane.start + 1..pane.end + 1].partition_point(|edge| *edge <= left);
        let last = pane.start + self.0[pane.start..pane.end].partition_point(|edge| *edge < right);
        let visible = first..last.max(first);
        ColumnViewport {
            layout: self.clone(),
            pane,
            visible,
            number,
            total_width,
        }
    }
}

#[derive(Clone)]
struct ColumnViewport {
    layout: ColumnLayout,
    pane: std::ops::Range<usize>,
    visible: std::ops::Range<usize>,
    number: bool,
    total_width: f32,
}

impl ColumnViewport {
    fn leading_width(&self) -> f32 {
        self.layout.width(self.pane.start..self.visible.start)
    }

    fn trailing_width(&self) -> f32 {
        self.layout.width(self.visible.end..self.pane.end)
    }

    fn selection_span(&self, region: SelectionRegion, include_number: bool) -> (f32, f32) {
        let gutter = if self.number { 52. } else { 0. };
        let left = gutter + self.layout.width(self.pane.start..region.left);
        let width = self.layout.width(region.left..region.right + 1);
        if self.number && include_number && region.left == self.pane.start {
            (0., gutter + width)
        } else {
            (left, width)
        }
    }

    /// Bounds and clamped offsets are committed during prepaint. If resizing,
    /// opening the sidebar or a width change made the previous frame's bounds
    /// stale, request one corrected render; stable geometry never re-notifies.
    fn observe_bounds(
        &self,
        handle: ScrollHandle,
        cx: &mut Context<Viewer>,
    ) -> impl Fn(Vec<Bounds<Pixels>>, &mut Window, &mut App) + 'static + use<> {
        let used = self.clone();
        let weak = cx.entity().downgrade();
        move |_, window, cx| {
            let width = f32::from(handle.bounds().size.width);
            if width <= 0. {
                return;
            }
            let actual = used.layout.viewport(
                used.pane.clone(),
                used.number,
                -f32::from(handle.offset().x),
                width,
            );
            if actual.visible != used.visible {
                let weak = weak.clone();
                window.defer(cx, move |_, cx| {
                    let _ = weak.update(cx, |_, cx| cx.notify());
                });
            }
        }
    }
}

fn column_spacer(width: f32) -> gpui::Div {
    div().w(px(width)).h_full().flex_shrink_0()
}

/// Visible interval along one screen axis. Only the selection's true outer
/// edges carry ink. A partially scrolled edge retains its visible thickness.
#[derive(Clone, Copy, Debug, PartialEq)]
struct OutlineSpan {
    start: Pixels,
    end: Pixels,
    leading: Pixels,
    trailing: Pixels,
}

impl OutlineSpan {
    fn clipped(
        start: Pixels,
        end: Pixels,
        viewport: std::ops::Range<Pixels>,
        leading: bool,
        trailing: bool,
    ) -> Option<Self> {
        let visible_start = start.max(viewport.start);
        let visible_end = end.min(viewport.end);
        if visible_start >= visible_end {
            return None;
        }
        let stroke = px(SELECTION_STROKE);
        Some(Self {
            start: visible_start,
            end: visible_end,
            leading: if leading {
                ((start + stroke).min(visible_end) - visible_start).max(px(0.))
            } else {
                px(0.)
            },
            trailing: if trailing {
                (visible_end - (end - stroke).max(visible_start)).max(px(0.))
            } else {
                px(0.)
            },
        })
    }

    // Adjacent panes can have a divider between their content rectangles.
    // Join across that divider and discard their shared interior edges.
    fn join(self, next: Self) -> Self {
        Self {
            end: next.end,
            trailing: next.trailing,
            ..self
        }
    }
}

/// Device-aligned solid strips also cover the corners when only a fraction of
/// a clipped edge remains. Nothing is painted over the interior of the selection.
fn outline_strips(x: OutlineSpan, y: OutlineSpan, scale: f32) -> [Bounds<Pixels>; 4] {
    let snap = |v: Pixels| px((f32::from(v) * scale).round() / scale);
    let rect = |l, t, r, b| Bounds::from_corners(point(snap(l), snap(t)), point(snap(r), snap(b)));
    [
        rect(x.start, y.start, x.start + x.leading, y.end),
        rect(x.end - x.trailing, y.start, x.end, y.end),
        rect(x.start, y.start, x.end, y.start + y.leading),
        rect(x.start, y.end - y.trailing, x.end, y.end),
    ]
}

impl ColumnViewport {
    fn selected_x(
        &self,
        region: SelectionRegion,
        include_number: bool,
        viewport: std::ops::Range<Pixels>,
        offset: Pixels,
    ) -> Option<OutlineSpan> {
        let start = region.left.max(self.pane.start);
        let end = (region.right + 1).min(self.pane.end);
        let (x, width, leading, trailing) = if start < end {
            let fragment = SelectionRegion {
                left: start,
                right: end - 1,
                ..region
            };
            let (x, width) = self.selection_span(fragment, include_number);
            (
                x,
                width,
                start == region.left && (!include_number || self.number),
                end - 1 == region.right,
            )
        } else if include_number && self.number && self.pane.is_empty() {
            (0., 52., true, false)
        } else {
            return None;
        };
        let left = viewport.start + offset + px(x);
        OutlineSpan::clipped(left, left + px(width), viewport, leading, trailing)
    }
}

fn selected_y(
    region: SelectionRegion,
    first_row: usize,
    rows: &ListState,
    include_header: bool,
) -> Option<OutlineSpan> {
    let start = region.top.max(first_row);
    let end = (region.bottom + 1).min(first_row + rows.item_count());
    if start >= end {
        return None;
    }
    let first_visible = rows.logical_scroll_top().item_ix;
    if end - first_row <= first_visible {
        return None;
    }
    let viewport = rows.viewport_bounds();
    let top = if start - first_row < first_visible {
        None
    } else {
        // A first selected row below the rendered items is entirely off screen.
        Some(rows.bounds_for_item(start - first_row)?.top())
    };
    let bottom = rows
        .bounds_for_item(end - 1 - first_row)
        .map(|bounds| bounds.bottom());
    OutlineSpan::clipped(
        top.unwrap_or(viewport.top()),
        bottom.unwrap_or(viewport.bottom()),
        viewport.top()..viewport.bottom(),
        !include_header && start == region.top && top.is_some(),
        end - 1 == region.bottom && bottom.is_some(),
    )
}

impl Viewer {
    fn table_selection_outline(
        &self,
        left: &ColumnViewport,
        body: &ColumnViewport,
        color: u32,
    ) -> Option<gpui::AnyElement> {
        let row_count = self.view_rows.len();
        let region = selection_region(
            self.selection,
            0..row_count.max(1),
            0..self.view_columns.len(),
        )?;
        let include_number = self.selection_kind == SelectionKind::Rows;
        let include_header = self.selection_kind == SelectionKind::Columns;
        let columns = [
            (left.clone(), self.frozen_horizontal.clone()),
            (body.clone(), self.horizontal.clone()),
        ];
        let rows = [
            (0, self.pinned_scroll.main.clone()),
            (self.frozen_row_count, self.scroll.main.clone()),
        ];

        // Paint once after all visible rows and headers. ListState now has the
        // actual row positions for this frame, including wrapped and scrolled rows.
        Some(
            canvas(
                |_, _, _| (),
                move |table, _, window, _| {
                    let x = columns
                        .iter()
                        .filter_map(|(pane, scroll)| {
                            let viewport = scroll.bounds();
                            pane.selected_x(
                                region,
                                include_number,
                                viewport.left()..viewport.right(),
                                scroll.offset().x,
                            )
                        })
                        .reduce(OutlineSpan::join);
                    let header = include_header.then(|| OutlineSpan {
                        start: table.top(),
                        end: table.top() + px(TABLE_HEADER_HEIGHT),
                        leading: px(SELECTION_STROKE),
                        trailing: if row_count == 0 {
                            px(SELECTION_STROKE)
                        } else {
                            px(0.)
                        },
                    });
                    let y = header
                        .into_iter()
                        .chain(rows.iter().filter_map(|(first, list)| {
                            selected_y(region, *first, list, include_header)
                        }))
                        .reduce(OutlineSpan::join);
                    if let (Some(x), Some(y)) = (x, y) {
                        for strip in outline_strips(x, y, window.scale_factor()) {
                            if strip.size.width > px(0.) && strip.size.height > px(0.) {
                                window.paint_quad(gpui::fill(strip, rgb(color)));
                            }
                        }
                    }
                },
            )
            .absolute()
            .inset_0()
            .into_any_element(),
        )
    }

    pub(super) fn render_table(
        &mut self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let Some(result) = self.result.clone() else {
            return self.render_welcome(cx);
        };
        let c = Colors::new(self.preferences.dark);
        self.ensure_row_layout(&result);
        let columns = self.view_columns.len();
        let frozen = self.frozen_count.min(columns);
        let column_layout = ColumnLayout::new(&self.view_columns, &self.widths);
        let frozen_width = 52. + column_layout.width(0..frozen);
        let body_width = column_layout.width(frozen..columns);
        let available =
            (f32::from(window.viewport_size().width) - self.right_sidebar_width(window) - 26.)
                .max(240.);
        // The outer separator occupies one pixel of this pane's width.
        let left_width = (frozen_width + 1.).min((available - 180.).max(53.));
        let pane_width = |handle: &ScrollHandle, fallback: f32| {
            let measured = f32::from(handle.bounds().size.width);
            if measured > 0. { measured } else { fallback }
        };
        let left_columns = column_layout.viewport(
            0..frozen,
            true,
            -f32::from(self.frozen_horizontal.offset().x),
            pane_width(&self.frozen_horizontal, left_width - 1.),
        );
        let body_columns = column_layout.viewport(
            frozen..columns,
            false,
            -f32::from(self.horizontal.offset().x),
            pane_width(&self.horizontal, available - left_width),
        );
        if let Some(browse) = &mut self.csv_browse {
            let fields = left_columns
                .visible
                .clone()
                .chain(body_columns.visible.clone())
                .map(|index| self.view_columns[index])
                .collect();
            browse.begin_viewport(fields);
        }
        let outline = self.table_selection_outline(&left_columns, &body_columns, c.accent);
        let pinned_height = if self.frozen_row_count == 0 {
            0.
        } else {
            let limit = ((f32::from(window.viewport_size().height) - 190.) * 0.42).clamp(56., 280.);
            let total = self
                .view_rows
                .frozen()
                .iter()
                // Every row is at least TABLE_ROW_MIN_HEIGHT. Once this many
                // rows fill the capped pane, further frozen rows cannot change
                // its height; never scan a large frozen selection per frame.
                .take((limit / TABLE_ROW_MIN_HEIGHT).ceil() as usize)
                .map(|row| {
                    f32::from(
                        self.row_heights
                            .get(row)
                            .copied()
                            .unwrap_or(px(TABLE_ROW_MIN_HEIGHT)),
                    )
                })
                .sum::<f32>();
            total.min(limit)
        };
        div()
            .relative()
            .size_full()
            .flex()
            // Reserve the scrollbar track so the last column and its outline
            // can be scrolled fully into view.
            .pr(px(12.))
            .child(
                div()
                    .w(px(left_width))
                    .flex_shrink_0()
                    .h_full()
                    .flex()
                    .flex_col()
                    .border_r_1()
                    .border_color(rgb(c.border))
                    .child(
                        div()
                            .on_children_prepainted(
                                left_columns.observe_bounds(self.frozen_horizontal.clone(), cx),
                            )
                            .id("frozen-columns-horizontal")
                            .flex_1()
                            .min_h_0()
                            .overflow_x_scroll()
                            .overflow_y_hidden()
                            .track_scroll(&self.frozen_horizontal)
                            .child(
                                div()
                                    .w(px(frozen_width))
                                    .h_full()
                                    .flex()
                                    .flex_col()
                                    .child(self.render_header(&left_columns, cx))
                                    .when(pinned_height > 0., |pane| {
                                        pane.child(self.render_row_band(
                                            true,
                                            left_columns.clone(),
                                            pinned_height,
                                            cx,
                                        ))
                                    })
                                    .child(self.render_row_band(false, left_columns, 0., cx)),
                            ),
                    )
                    .child(self.render_scrollbar(
                        true,
                        frozen_width,
                        TableScrollbar::Pixels(self.frozen_horizontal.clone()),
                        3,
                        cx,
                    )),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .on_children_prepainted(
                                body_columns.observe_bounds(self.horizontal.clone(), cx),
                            )
                            .id("table-horizontal")
                            .flex_1()
                            .min_h_0()
                            .overflow_x_scroll()
                            .overflow_y_hidden()
                            .track_scroll(&self.horizontal)
                            .map(|mut el| {
                                el.style().restrict_scroll_to_axis = Some(true);
                                el
                            })
                            .child(
                                div()
                                    .w(px(body_width))
                                    .min_w_full()
                                    .h_full()
                                    .flex()
                                    .flex_col()
                                    .child(self.render_header(&body_columns, cx))
                                    .when(pinned_height > 0., |pane| {
                                        pane.child(self.render_row_band(
                                            true,
                                            body_columns.clone(),
                                            pinned_height,
                                            cx,
                                        ))
                                    })
                                    .child(self.render_row_band(false, body_columns, 0., cx)),
                            ),
                    )
                    .child(self.render_scrollbar(
                        true,
                        body_width,
                        TableScrollbar::Pixels(self.horizontal.clone()),
                        0,
                        cx,
                    )),
            )
            .children(outline)
            .when(pinned_height > 0., |table| {
                table.child(
                    div()
                        .absolute()
                        .right_0()
                        .top(px(TABLE_HEADER_HEIGHT))
                        .h(px(pinned_height))
                        .w(px(12.))
                        .child(self.render_scrollbar(
                            false,
                            0.,
                            TableScrollbar::Rows(self.pinned_scroll.clone()),
                            2,
                            cx,
                        )),
                )
            })
            .child(
                div()
                    .absolute()
                    .right_0()
                    .top(px(TABLE_HEADER_HEIGHT + pinned_height))
                    .bottom(px(12.))
                    .w(px(12.))
                    .child(self.render_scrollbar(
                        false,
                        0.,
                        TableScrollbar::Rows(self.scroll.clone()),
                        1,
                        cx,
                    )),
            )
            .into_any_element()
    }

    fn render_row_band(
        &mut self,
        pinned: bool,
        columns: ColumnViewport,
        height: f32,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let c = Colors::new(self.preferences.dark);
        let number = columns.number;
        let rows = if pinned {
            self.pinned_scroll.clone()
        } else {
            self.scroll.clone()
        };
        let state = if number {
            rows.frozen.clone()
        } else {
            rows.main.clone()
        };
        let offset = if pinned { 0 } else { self.frozen_row_count };
        let empty = state.item_count() == 0;
        div()
            .id(match (pinned, number) {
                (true, true) => "pinned-left",
                (true, false) => "pinned-main",
                (false, true) => "body-left",
                (false, false) => "body-main",
            })
            .relative()
            .min_h_0()
            .w_full()
            .when(pinned, |el| el.h(px(height)).flex_shrink_0())
            .when(!pinned, |el| el.flex_1())
            .on_scroll_wheel(cx.listener(move |_, _, _, cx| {
                rows.sync_from(number);
                cx.notify();
            }))
            .child(
                list(
                    state,
                    cx.processor(move |this, index, window, cx| {
                        this.render_row(index + offset, &columns, window, cx)
                            .into_any_element()
                    }),
                )
                .size_full(),
            )
            .when(pinned, |el| {
                // Keep the last two pixels of the last frozen row in its viewport.
                el.child(
                    div()
                        .absolute()
                        .left_0()
                        .right_0()
                        .bottom_0()
                        .h(px(2.))
                        .bg(rgb(c.border)),
                )
            })
            .when(empty && !number && !pinned, |el| {
                el.child(
                    div()
                        .absolute()
                        .top_8()
                        .left_8()
                        .text_color(rgb(c.muted))
                        .child(t(if self.view_rows.is_empty() {
                            "No records match the current filter."
                        } else {
                            "All rows are frozen."
                        })),
                )
            })
            .into_any_element()
    }

    fn render_header(&self, columns: &ColumnViewport, cx: &mut Context<Self>) -> gpui::Div {
        let c = Colors::new(self.preferences.dark);
        let result = self.result.as_ref().unwrap();
        let number = columns.number;
        div()
            .relative()
            .flex()
            .w(px(columns.total_width))
            .h(px(TABLE_HEADER_HEIGHT))
            .flex_shrink_0()
            .bg(rgb(c.header))
            .when(number, |el| {
                el.child(
                    div()
                        .id("select-table")
                        .w(px(52.))
                        .h_full()
                        .flex_shrink_0()
                        .border_r_1()
                        .border_b_1()
                        .border_color(rgb(c.grid))
                        .flex()
                        .items_center()
                        .justify_center()
                        .cursor_pointer()
                        .child("#")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.focus.focus(window);
                            this.select_all(cx);
                        })),
                )
            })
            .child(column_spacer(columns.leading_width()))
            .children(columns.visible.clone().map(|index| {
                let source = self.source_column(index);
                let col = &result.columns[source];
                let name = col.name.clone();
                let frozen = index < self.frozen_count;
                let direction = self
                    .displayed_request
                    .sort_rules()
                    .enumerate()
                    .find(|(_, (field, _, _))| *field == name)
                    .map(|(priority, (_, descending, _))| {
                        let arrow = if descending { '▼' } else { '▲' };
                        if self.displayed_request.sort_keys.len() > 1 {
                            format!("{arrow}{}", priority + 1)
                        } else {
                            arrow.to_string()
                        }
                    });
                let selected = self.selection.is_some_and(|(a, b)| {
                    index >= a.1.min(b.1)
                        && index <= a.1.max(b.1)
                        && self.selection_kind == SelectionKind::Columns
                });
                let mark = self.table_state.column_mark(&name);
                div()
                    .id(("column", index))
                    .relative()
                    .w(px(self.widths.get(source).copied().unwrap_or(150.)))
                    .h_full()
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .border_r_1()
                    .border_b_1()
                    .border_color(rgb(c.grid))
                    .when_some(mark, |el, mark| {
                        el.bg(rgb(mark.background(self.preferences.dark)))
                    })
                    .when(selected, |el| {
                        let fill = rgb(mark.map_or(c.selected, |mark| {
                            mark.selection_background(self.preferences.dark)
                        }));
                        el.bg(fill).border_color(fill)
                    })
                    .cursor_pointer()
                    .when(!selected && mark.is_none(), |el| {
                        el.hover(|el| el.bg(rgb(c.hover)))
                    })
                    .when_some(mark, |el, mark| {
                        el.child(
                            div()
                                .absolute()
                                .left_0()
                                .right_0()
                                .bottom_0()
                                .h(px(3.))
                                .bg(rgb(mark.accent(self.preferences.dark))),
                        )
                    })
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .when(frozen, |el| el.text_color(rgb(c.frozen_text)))
                            .child(name.clone()),
                    )
                    .when_some(direction, |el, direction| {
                        el.child(
                            div()
                                .flex_shrink_0()
                                .text_color(rgb(c.accent))
                                .child(direction),
                        )
                    })
                    .when(frozen, |el| {
                        el.child(icons::glyph(Icon::Freeze, 11., self.preferences.dark))
                    })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                            this.begin_axis_selection(
                                Axis::Columns,
                                index,
                                event.modifiers.shift,
                                window,
                                cx,
                            );
                        }),
                    )
                    .on_mouse_move(cx.listener(move |this, event: &MouseMoveEvent, _, cx| {
                        if this.selecting
                            && this.selection_kind == SelectionKind::Columns
                            && event.dragging()
                        {
                            this.extend_selection(0, index, cx);
                        }
                    }))
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, _, window, cx| {
                            this.show_axis_menu(Axis::Columns, index, window, cx)
                        }),
                    )
                    .child(
                        div()
                            .id(("resize", index))
                            .absolute()
                            .right_0()
                            .top_0()
                            .w(px(6.))
                            .h_full()
                            .cursor_col_resize()
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                                    cx.stop_propagation();
                                    this.selecting = false;
                                    if event.click_count == 2 {
                                        this.auto_size_column(source, window, cx);
                                    } else {
                                        this.resize =
                                            Some((source, event.position.x, this.widths[source]));
                                    }
                                }),
                            ),
                    )
            }))
            .child(column_spacer(columns.trailing_width()))
    }

    fn render_row(
        &mut self,
        row: usize,
        columns: &ColumnViewport,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let c = Colors::new(self.preferences.dark);
        let number = columns.number;
        let result = self.result.as_ref().unwrap().clone();
        let source_row = self.source_row(row);
        let browse = self.csv_browse.is_some();
        let csv_row = if browse && !columns.visible.is_empty() {
            self.csv_display_row(source_row, cx)
        } else {
            None
        };
        let frozen_row = row < self.frozen_row_count;
        let first_measure =
            !self.wrapped_columns.is_empty() && !self.row_heights.contains_key(&source_row);
        let height = if browse {
            px(TABLE_ROW_MIN_HEIGHT)
        } else {
            self.measured_row_height(source_row, window)
        };
        if first_measure && frozen_row {
            cx.defer_in(window, |_, _, cx| cx.notify());
        }
        let row_mark = self.table_state.row_mark(source_row);
        let row_selected = self.selection.is_some_and(|(a, b)| {
            row >= a.0.min(b.0) && row <= a.0.max(b.0) && self.selection_kind == SelectionKind::Rows
        });
        div()
            .relative()
            .flex()
            .w(px(columns.total_width))
            .h(height)
            .bg(rgb(row_mark.map_or(
                if row % 2 == 0 { c.surface } else { c.stripe },
                |mark| mark.background(self.preferences.dark),
            )))
            .on_mouse_move(cx.listener(move |this, event: &MouseMoveEvent, _, cx| {
                if this.selecting && this.selection_kind == SelectionKind::Rows && event.dragging()
                {
                    this.extend_selection(row, 0, cx);
                }
            }))
            .when(number, |el| {
                el.child(
                    div()
                        .id(("row", row))
                        .relative()
                        .w(px(52.))
                        .h_full()
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .gap_1()
                        .text_size(px(11.))
                        .text_color(rgb(if frozen_row {
                            c.frozen_text
                        } else if self.displayed_request.filter.is_empty() {
                            c.muted
                        } else {
                            c.blue_text
                        }))
                        .border_r_1()
                        .border_b_1()
                        .border_color(rgb(c.grid))
                        .bg(rgb(row_mark.map_or(
                            if row % 2 == 0 { c.bar } else { c.header },
                            |mark| mark.background(self.preferences.dark),
                        )))
                        .when(row_selected, |el| {
                            let fill = rgb(row_mark.map_or(c.selected, |mark| {
                                mark.selection_background(self.preferences.dark)
                            }));
                            el.bg(fill).border_color(fill)
                        })
                        .when_some(row_mark, |el, mark| {
                            el.child(
                                div()
                                    .absolute()
                                    .left_0()
                                    .top_0()
                                    .bottom_0()
                                    .w(px(4.))
                                    .bg(rgb(mark.accent(self.preferences.dark))),
                            )
                        })
                        .cursor_pointer()
                        .child(self.row_number(source_row).to_string())
                        .when(frozen_row, |el| {
                            el.child(icons::glyph(Icon::Freeze, 9., self.preferences.dark))
                        })
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                                this.begin_axis_selection(
                                    Axis::Rows,
                                    row,
                                    event.modifiers.shift,
                                    window,
                                    cx,
                                )
                            }),
                        )
                        .on_mouse_down(
                            MouseButton::Right,
                            cx.listener(move |this, _, window, cx| {
                                this.show_axis_menu(Axis::Rows, row, window, cx)
                            }),
                        ),
                )
            })
            .child(column_spacer(columns.leading_width()))
            .children(columns.visible.clone().map(|col| {
                let source_col = self.source_column(col);
                let csv_cell = csv_row
                    .as_ref()
                    .and_then(|display| display.cell(source_col));
                let missing = if browse {
                    csv_cell.as_ref().is_some_and(|(is_null, _)| *is_null)
                } else {
                    result.is_null(source_row, source_col)
                };
                let nested = !browse && !missing && result.is_nested(source_row, source_col);
                let frozen = frozen_row || col < self.frozen_count;
                let audio = !browse && result.binary(source_row, source_col).is_some_and(is_audio);
                let wrapped = !browse
                    && self
                        .formats
                        .get(&result.columns[source_col].name)
                        .is_some_and(|style| style.word_wrap);
                let selected = self.selection.is_some_and(|(a, b)| {
                    row >= a.0.min(b.0)
                        && row <= a.0.max(b.0)
                        && col >= a.1.min(b.1)
                        && col <= a.1.max(b.1)
                });
                let mark = row_mark.or_else(|| {
                    self.table_state
                        .column_mark(&result.columns[source_col].name)
                });
                div()
                    .id(("cell", row * result.columns.len() + col))
                    .w(px(self.widths[source_col]))
                    .h_full()
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .border_r_1()
                    .border_b_1()
                    .border_color(rgb(c.grid))
                    .overflow_hidden()
                    .when_some(mark, |el, mark| {
                        el.bg(rgb(mark.background(self.preferences.dark)))
                    })
                    .when(selected, |el| {
                        let fill = rgb(mark.map_or(c.selected, |mark| {
                            mark.selection_background(self.preferences.dark)
                        }));
                        el.bg(fill).border_color(fill)
                    })
                    .when(!selected && mark.is_none(), |el| {
                        el.hover(|el| el.bg(rgb(c.hover)))
                    })
                    .text_color(rgb(if frozen && !missing {
                        c.frozen_text
                    } else {
                        c.value_foreground(missing, nested)
                    }))
                    .text_size(px(12.))
                    .line_height(px(TABLE_TEXT_LINE_HEIGHT))
                    .when(!audio, |el| {
                        el.when(nested, |el| {
                            el.child(icons::glyph(Icon::Table, 14., self.preferences.dark))
                        })
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .when(!wrapped, |el| el.truncate())
                                .when(wrapped, |el| el.whitespace_normal())
                                .child(if browse {
                                    match csv_cell {
                                        Some((_, text)) => {
                                            let info = &result.columns[source_col];
                                            self.formats
                                                .get(&info.name)
                                                .and_then(|s| s.float.apply(&text, &info.data_type))
                                                .map(SharedString::from)
                                                .unwrap_or(text)
                                        }
                                        None if csv_row.is_some() => "—".into(),
                                        None => "…".into(),
                                    }
                                } else {
                                    self.row_text(source_row, source_col, wrapped)
                                }),
                        )
                    })
                    .when(audio, |el| {
                        el.child(self.inline_audio_controls(source_row, source_col, cx))
                    })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                            this.focus.focus(window);
                            this.selection_kind = SelectionKind::Cells;
                            let anchor = if event.modifiers.shift {
                                this.selection.map(|(a, _)| a).unwrap_or((row, col))
                            } else {
                                (row, col)
                            };
                            this.selection = Some((anchor, (row, col)));
                            this.selecting = true;
                            if event.click_count == 2 && browse {
                                this.selecting = false;
                                this.inspect_csv_browse_selection(cx);
                            } else if event.click_count == 2 {
                                let preview = this.result.as_ref().and_then(|result| {
                                    if source_row >= result.row_count()
                                        || source_col >= result.columns.len()
                                    {
                                        None
                                    } else {
                                        preview_kind(
                                            result.is_null(source_row, source_col),
                                            result.binary(source_row, source_col),
                                            result.is_nested(source_row, source_col),
                                        )
                                    }
                                });
                                if let Some(preview) = preview {
                                    this.selecting = false;
                                    this.open_peek(
                                        source_row,
                                        source_col,
                                        preview == CellPreview::Image,
                                        cx,
                                    );
                                }
                            }
                            cx.notify();
                        }),
                    )
                    .on_mouse_move(cx.listener(move |this, event: &MouseMoveEvent, _, cx| {
                        if this.selecting && event.dragging() {
                            this.extend_selection(row, col, cx);
                        }
                    }))
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, _, window, cx| {
                            this.show_selection_menu(row, col, window, cx)
                        }),
                    )
                    .on_mouse_down(
                        MouseButton::Middle,
                        cx.listener(move |this, _, _, cx| {
                            if this.result.as_ref().is_some_and(|result| {
                                source_row < result.row_count()
                                    && source_col < result.columns.len()
                                    && !result.is_null(source_row, source_col)
                                    && result.is_nested(source_row, source_col)
                                    && result
                                        .binary(source_row, source_col)
                                        .is_some_and(|bytes| image_format(bytes).is_some())
                            }) {
                                this.open_peek(source_row, source_col, true, cx);
                            }
                        }),
                    )
            }))
            .child(column_spacer(columns.trailing_width()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn selection_fragments_reject_empty_and_outside_panes_and_normalize_single_cells() {
        assert_eq!(selection_region(None, 0..5, 0..5), None);
        let single = Some(((3, 2), (3, 2)));
        assert_eq!(
            selection_region(single, 0..5, 0..5),
            Some(SelectionRegion {
                top: 3,
                bottom: 3,
                left: 2,
                right: 2,
            })
        );
        assert_eq!(selection_region(single, 0..3, 0..5), None);
        assert_eq!(selection_region(single, 0..5, 3..5), None);
        assert_eq!(selection_region(single, 0..0, 0..5), None);
        assert_eq!(selection_region(single, 0..5, 0..0), None);
        assert_eq!(
            selection_region(
                Some(((usize::MAX, usize::MAX), (usize::MAX, usize::MAX))),
                0..5,
                0..5
            ),
            None
        );
    }

    #[test]
    fn reverse_rectangle_intersects_each_pane_without_overlapping_cells() {
        let selection = Some(((4, 5), (1, 1)));
        let mut covered = std::collections::HashSet::new();
        for (rows, columns) in [(0..2, 0..3), (0..2, 3..8), (2..7, 0..3), (2..7, 3..8)] {
            let region = selection_region(selection, rows.clone(), columns.clone()).unwrap();
            assert!(rows.contains(&region.top) && rows.contains(&region.bottom));
            assert!(columns.contains(&region.left) && columns.contains(&region.right));
            assert_eq!(region.top, if rows.start == 0 { 1 } else { 2 });
            assert_eq!(region.bottom, if rows.start == 0 { 1 } else { 4 });
            assert_eq!(region.left, if columns.start == 0 { 1 } else { 3 });
            assert_eq!(region.right, if columns.start == 0 { 2 } else { 5 });
            for row in region.top..=region.bottom {
                for column in region.left..=region.right {
                    assert!(covered.insert((row, column)), "Overlapping pane fragments");
                }
            }
        }
        let expected = (1..=4)
            .flat_map(|row| (1..=5).map(move |column| (row, column)))
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(covered, expected);
    }

    #[test]
    fn selection_pixel_span_follows_reordered_widths_and_row_number_membership() {
        let widths = [90., 120., 175., 80.];
        let order = [2, 0, 3, 1];
        let layout = ColumnLayout::new(&order, &widths);
        let left = layout.viewport(0..2, true, 0., 500.);
        let body = layout.viewport(2..4, false, 0., 500.);
        let region = SelectionRegion {
            top: 1,
            bottom: 3,
            left: 1,
            right: 1,
        };
        // The first unselected display column is source 2 (175px), and
        // the selected display column is source 0 (90px), after the 52px gutter.
        assert_eq!(left.selection_span(region, false), (227., 90.));
        let frozen = SelectionRegion {
            left: 0,
            right: 1,
            ..region
        };
        assert_eq!(left.selection_span(frozen, false), (52., 265.));
        assert_eq!(left.selection_span(frozen, true), (0., 317.));
        let regular = SelectionRegion {
            left: 2,
            right: 3,
            ..region
        };
        assert_eq!(body.selection_span(regular, true), (0., 200.));
    }

    #[test]
    fn outline_joins_header_frozen_and_scrolling_rows_with_one_device_aligned_perimeter() {
        let span = |a, b, first, last| {
            OutlineSpan::clipped(px(a), px(b), px(a)..px(b), first, last).unwrap()
        };
        let x = span(42.25, 98., true, false).join(span(99., 150.25, false, true));
        let y = span(25.25, 57.25, true, false)
            .join(span(57.25, 113.25, false, false))
            .join(span(115.25, 184.25, false, true));
        for scale in [1., 2.] {
            let [left, right, top, bottom] = outline_strips(x, y, scale);
            assert_eq!(left.size.width, px(SELECTION_STROKE));
            assert_eq!(right.size.width, left.size.width);
            assert_eq!(top.size.height, left.size.width);
            assert_eq!(bottom.size.height, left.size.width);
            assert_eq!(left.origin, top.origin);
            assert_eq!(left.bottom(), bottom.bottom());
            assert_eq!(top.right(), right.right());
            assert_eq!(right.bottom_right(), bottom.bottom_right());
        }
    }

    #[test]
    fn outline_clips_true_edges_and_includes_the_row_gutter_without_frozen_columns() {
        let layout = ColumnLayout::new(&[0, 1, 2], &[80., 100., 120.]);
        let gutter = layout.viewport(0..0, true, 0., 52.);
        let body = layout.viewport(0..3, false, 20., 200.);
        let region = selection_region(Some(((0, 0), (1, 2))), 0..2, 0..3).unwrap();
        assert!(
            gutter
                .selected_x(region, false, px(0.)..px(52.), px(0.))
                .is_none()
        );
        let left = gutter
            .selected_x(region, true, px(0.)..px(52.), px(0.))
            .unwrap();
        let right = body
            .selected_x(region, true, px(53.)..px(253.), px(-20.))
            .unwrap();
        assert_eq!(
            left.join(right),
            OutlineSpan {
                start: px(0.),
                end: px(253.),
                leading: px(2.),
                trailing: px(0.)
            }
        );
        let clipped = OutlineSpan::clipped(px(1.), px(100.), px(2.5)..px(99.), true, true).unwrap();
        assert_eq!((clipped.leading, clipped.trailing), (px(0.5), px(1.)));
        assert!(OutlineSpan::clipped(px(1.), px(2.), px(5.)..px(10.), true, true).is_none());
    }

    #[test]
    fn thousand_column_view_builds_only_visible_columns_and_pixel_overscan() {
        let order: Vec<_> = (0..1000).collect();
        let layout = ColumnLayout::new(&order, &[100.; 1000]);
        let viewport = layout.viewport(0..1000, false, 50_000., 500.);
        assert_eq!(viewport.visible, 498..507);
        assert_eq!(viewport.visible.len(), 9);
        assert_eq!(viewport.leading_width(), 49_800.);
        assert_eq!(viewport.trailing_width(), 49_300.);
        assert_eq!(
            viewport.leading_width()
                + layout.width(viewport.visible.clone())
                + viewport.trailing_width(),
            100_000.
        );
        assert_eq!(layout.viewport(0..1000, false, 0., 500.).visible, 0..7);
        assert_eq!(
            layout.viewport(0..1000, false, f32::MAX, 500.).visible,
            993..1000
        );
    }

    #[test]
    fn frozen_and_body_viewports_use_their_own_scroll_origin_and_gutter() {
        let order: Vec<_> = (0..1000).rev().collect();
        let layout = ColumnLayout::new(&order, &[100.; 1000]);
        let frozen = layout.viewport(0..20, true, 700., 200.);
        let body = layout.viewport(20..1000, false, 50_000., 500.);
        assert_eq!(frozen.visible, 5..10);
        assert_eq!(body.visible, 518..527);
        assert_eq!(frozen.total_width, 2052.);
        assert_eq!(body.total_width, 98_000.);
        assert!(frozen.visible.clone().all(|column| column < 20));
        assert!(body.visible.clone().all(|column| column >= 20));
        // Display indices remain unchanged: source identity is mapped only
        // after choosing visible display columns, including non-prefix pins.
        assert_eq!(order[frozen.visible.start], 994);
        assert_eq!(order[body.visible.start], 481);
        let empty = layout.viewport(20..20, false, 0., 500.);
        assert!(empty.visible.is_empty());
        assert_eq!(empty.leading_width() + empty.trailing_width(), 0.);
    }

    #[test]
    fn variable_width_resize_and_reordered_pins_match_geometric_intersections() {
        let order = [7, 2, 5, 0, 9, 1, 8, 3, 6, 4];
        let mut widths = [70., 90., 330., 120., 150., 810., 200., 80., 400., 110.];
        for resized in [70., 1500., 95.] {
            widths[2] = resized;
            let layout = ColumnLayout::new(&order, &widths);
            for (pane, number, width) in [(0..3, true, 185.), (3..10, false, 360.)] {
                for requested in [0., 80., 420., 1200., 20_000.] {
                    let view = layout.viewport(pane.clone(), number, requested, width);
                    // Independent linear rectangle intersection is the oracle,
                    // rather than repeating the binary-search implementation.
                    let gutter = if number { 52. } else { 0. };
                    let total = gutter + pane.clone().map(|i| widths[order[i]]).sum::<f32>();
                    let scroll = requested.clamp(0., (total - width).max(0.));
                    let mut x = gutter;
                    let expected: Vec<_> = pane
                        .clone()
                        .filter(|&i| {
                            let left = x;
                            x += widths[order[i]];
                            x > scroll - COLUMN_OVERSCAN && left < scroll + width + COLUMN_OVERSCAN
                        })
                        .collect();
                    assert_eq!(view.visible.clone().collect::<Vec<_>>(), expected);
                    assert_eq!(
                        gutter
                            + view.leading_width()
                            + layout.width(view.visible.clone())
                            + view.trailing_width(),
                        total
                    );
                }
            }
        }
    }

    #[test]
    fn virtual_range_never_turns_offscreen_selection_edges_into_visible_edges() {
        let order: Vec<_> = (0..1000).collect();
        let layout = ColumnLayout::new(&order, &[100.; 1000]);
        let view = layout.viewport(0..1000, false, 50_000., 500.);
        let region = selection_region(Some(((2, 900), (0, 10))), 0..10, view.pane.clone()).unwrap();
        assert_eq!(view.visible, 498..507);
        assert_eq!(view.selection_span(region, false), (1000., 89_100.));
        // Both true vertical edges are outside the 500px viewport and remain
        // there for clipping; the buffered range must not invent new edges.
        let (left, width) = view.selection_span(region, false);
        assert!(left < 50_000. && left + width > 50_500.);
        assert_eq!(region.left, 10);
        assert_eq!(region.right, 900);
    }

    #[test]
    fn double_click_prefers_real_image_bytes_for_binary_and_photo_structs() {
        let mut cell = Cell {
            bytes: Some(b"\x89PNG\r\n\x1a\n".to_vec()),
            ..Default::default()
        };
        assert_eq!(double_click_preview(&cell), Some(CellPreview::Image));
        cell.json = Some(json!({"bytes":"encoded display bytes", "path":"photo.png"}));
        assert_eq!(double_click_preview(&cell), Some(CellPreview::Image));
        cell.bytes = Some(b"\xff\xd8\xff\xe0".to_vec());
        assert_eq!(double_click_preview(&cell), Some(CellPreview::Image));
        // NULL state wins even if a malformed/stale caller supplied bytes.
        cell.is_null = true;
        assert_eq!(double_click_preview(&cell), None);
    }

    #[test]
    fn double_click_keeps_lists_and_structs_nested_without_guessing_photo_paths() {
        for value in [
            json!([1, null, {"child":[2]}]),
            json!({"name":"普通结构", "children":[{"value":1}]}),
            json!({"bytes":null, "path":"external-only.png"}),
            json!({"bytes":"89504e470d0a1a0a", "path":"not-decoded.png"}),
        ] {
            let cell = Cell {
                json: Some(value),
                ..Default::default()
            };
            assert_eq!(double_click_preview(&cell), Some(CellPreview::Nested));
        }
    }

    #[test]
    fn double_click_does_not_open_ordinary_scalars_or_non_image_binary() {
        for cell in [
            Cell::default(),
            Cell {
                text: "photo.png".into(),
                ..Default::default()
            },
            Cell {
                bytes: Some(vec![0, 1, 2, 3]),
                ..Default::default()
            },
            Cell {
                json: Some(json!(42)),
                ..Default::default()
            },
            Cell {
                json: Some(json!("scalar JSON")),
                ..Default::default()
            },
            Cell {
                is_null: true,
                json: Some(json!([])),
                ..Default::default()
            },
        ] {
            assert_eq!(double_click_preview(&cell), None);
        }
    }
}
