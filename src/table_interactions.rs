//! Axis selection and view-only table actions. Selection coordinates are display
//! indices; result data, widths, formatting and media identities stay unchanged.
use super::*;
use crate::engine::SortKey;
use crate::table_state::MarkColor;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum SelectionKind {
    #[default]
    Cells,
    Rows,
    Columns,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Axis {
    Rows,
    Columns,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct AxisMenu {
    pub(super) position: gpui::Point<Pixels>,
    pub(super) axis: Axis,
}

#[derive(Clone, Copy)]
enum AxisCommand {
    Copy { headers: bool },
    AutoFit,
    Sort(bool),
    SortOptions,
    Freeze,
    Unfreeze,
    UnfreezeAll,
    Mark(Option<MarkColor>),
    WordWrap(bool),
    ColumnDisplay { column: usize, binary: bool },
}

struct AxisMenuItem {
    label: &'static str,
    command: Option<AxisCommand>,
    enabled: bool,
    checked: bool,
    color: Option<MarkColor>,
}

impl AxisMenuItem {
    fn action(label: &'static str, command: AxisCommand, enabled: bool) -> Self {
        Self {
            label,
            command: Some(command),
            enabled,
            checked: false,
            color: None,
        }
    }

    fn separator() -> Self {
        Self {
            label: "",
            command: None,
            enabled: false,
            checked: false,
            color: None,
        }
    }
}

impl Viewer {
    /// Callers pass an in-bounds display index. The identity fallback also keeps
    /// the initial, not-yet-refreshed view usable during snapshot publication.
    pub(super) fn source_row(&self, view: usize) -> usize {
        self.view_rows.get(view).unwrap_or(view)
    }

    pub(super) fn source_column(&self, view: usize) -> usize {
        self.view_columns.get(view).copied().unwrap_or(view)
    }

    pub(super) fn view_row(&self, source: usize) -> Option<usize> {
        if self.view_rows.is_empty() {
            self.result
                .as_ref()
                .filter(|_| source < self.table_row_count())
                .map(|_| source)
        } else {
            self.view_rows.view(source)
        }
    }

    pub(super) fn view_column(&self, source: usize) -> Option<usize> {
        if self.view_columns.is_empty() {
            self.result
                .as_ref()
                .filter(|result| source < result.columns.len())
                .map(|_| source)
        } else {
            self.view_columns
                .iter()
                .position(|column| *column == source)
        }
    }

    pub(super) fn selected_source_rows(&self) -> Vec<usize> {
        let Some((start, end)) = self.selection else {
            return Vec::new();
        };
        let rows = self.table_row_count();
        if rows == 0 {
            return Vec::new();
        }
        let last = start.0.max(end.0).min(rows - 1);
        (start.0.min(end.0)..=last)
            .map(|row| self.source_row(row))
            .filter(|row| *row < rows)
            .collect()
    }

    pub(super) fn selected_source_columns(&self) -> Vec<usize> {
        let (Some(result), Some((start, end))) = (&self.result, self.selection) else {
            return Vec::new();
        };
        if result.columns.is_empty() {
            return Vec::new();
        }
        let last = start.1.max(end.1).min(result.columns.len() - 1);
        (start.1.min(end.1)..=last)
            .map(|column| self.source_column(column))
            .filter(|column| *column < result.columns.len())
            .collect()
    }

    pub(super) fn refresh_table_order(&mut self, cx: &mut Context<Self>) {
        if let Some(result) = &self.result {
            let rows = self.table_row_count();
            let names: Vec<_> = result
                .columns
                .iter()
                .map(|column| column.name.clone())
                .collect();
            let available = self.info.as_ref().map(|info| {
                info.columns
                    .iter()
                    .map(|column| column.name.clone())
                    .collect::<Vec<_>>()
            });
            self.table_state
                .reconcile(rows, available.as_deref().unwrap_or(&names));
            self.view_rows = self.table_state.row_order(rows);
            self.frozen_row_count = self.view_rows.frozen().len();
            let (mut columns, body) = self.table_state.column_order(&names);
            self.frozen_count = columns.len();
            columns.extend(body);
            self.view_columns = columns;
        } else {
            self.view_rows.clear();
            self.view_columns.clear();
            self.frozen_row_count = 0;
            self.frozen_count = 0;
        }
        self.selection = None;
        self.selection_kind = SelectionKind::Cells;
        self.selection_menu = None;
        self.axis_menu = None;
        self.selecting = false;
        self.resize = None;
        self.scroll_drag = None;
        self.audio_seek_drag = false;
        self.scroll.scrollbar_drag_ended();
        self.pinned_scroll.scrollbar_drag_ended();
        if self.csv_browse.is_some() {
            self.scroll.set_fixed_height(true);
            self.pinned_scroll.set_fixed_height(true);
            self.wrapped_columns.clear();
        }
        self.scroll
            .reset(self.view_rows.len().saturating_sub(self.frozen_row_count));
        self.pinned_scroll.reset(self.frozen_row_count);
        self.horizontal.set_offset(point(px(0.), px(0.)));
        self.frozen_horizontal.set_offset(point(px(0.), px(0.)));
        cx.notify();
    }

    pub(super) fn begin_axis_selection(
        &mut self,
        axis: Axis,
        index: usize,
        shift: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.busy.is_some() || self.panel.is_some() {
            return;
        }
        let Some(result) = &self.result else {
            return;
        };
        let (rows, columns) = (self.table_row_count(), result.columns.len());
        if columns == 0 || index >= if axis == Axis::Rows { rows } else { columns } {
            return;
        }
        let kind = if axis == Axis::Rows {
            SelectionKind::Rows
        } else {
            SelectionKind::Columns
        };
        let anchor = if shift && self.selection_kind == kind {
            self.selection
                .map(|(start, _)| if axis == Axis::Rows { start.0 } else { start.1 })
                .unwrap_or(index)
        } else {
            index
        };
        self.selection = Some(match axis {
            Axis::Rows => ((anchor.min(rows - 1), 0), (index, columns - 1)),
            Axis::Columns => (
                (0, anchor.min(columns - 1)),
                (rows.saturating_sub(1), index),
            ),
        });
        self.selection_kind = kind;
        self.selection_menu = None;
        self.axis_menu = None;
        self.selecting = true;
        self.focus.focus(window);
        cx.notify();
    }

    pub(super) fn extend_selection(&mut self, row: usize, column: usize, cx: &mut Context<Self>) {
        if !self.selecting || self.busy.is_some() || self.panel.is_some() {
            return;
        }
        let (Some(result), Some((anchor, _))) = (&self.result, self.selection) else {
            return;
        };
        let rows = self.table_row_count();
        if rows == 0 || result.columns.is_empty() {
            return;
        }
        let row = row.min(rows - 1);
        let column = column.min(result.columns.len() - 1);
        self.selection = Some(match self.selection_kind {
            SelectionKind::Cells => (anchor, (row, column)),
            SelectionKind::Rows => ((anchor.0, 0), (row, result.columns.len() - 1)),
            SelectionKind::Columns => ((0, anchor.1), (rows - 1, column)),
        });
        cx.notify();
    }

    pub(super) fn show_axis_menu(
        &mut self,
        axis: Axis,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.busy.is_some() || self.panel.is_some() {
            return;
        }
        let Some(result) = &self.result else {
            return;
        };
        let (rows, columns) = (self.table_row_count(), result.columns.len());
        if columns == 0 || index >= if axis == Axis::Rows { rows } else { columns } {
            return;
        }
        let opposite = if axis == Axis::Rows {
            SelectionKind::Columns
        } else {
            SelectionKind::Rows
        };
        let inside = self.selection_kind != opposite
            && self.selection.is_some_and(|(start, end)| {
                let (first, last) = if axis == Axis::Rows {
                    (start.0.min(end.0), start.0.max(end.0))
                } else {
                    (start.1.min(end.1), start.1.max(end.1))
                };
                (first..=last).contains(&index)
            });
        if inside {
            if let Some((start, end)) = self.selection {
                self.selection = Some(match axis {
                    Axis::Rows => ((start.0, 0), (end.0, columns - 1)),
                    Axis::Columns => ((0, start.1), (rows.saturating_sub(1), end.1)),
                });
            }
            self.selection_kind = if axis == Axis::Rows {
                SelectionKind::Rows
            } else {
                SelectionKind::Columns
            };
        } else {
            self.begin_axis_selection(axis, index, false, window, cx);
        }
        self.selecting = false;
        self.selection_menu = None;
        self.axis_menu = Some(AxisMenu {
            position: window.mouse_position(),
            axis,
        });
        self.focus.focus(window);
        cx.notify();
    }

    fn selected_column_names(&self) -> Vec<String> {
        let Some(result) = &self.result else {
            return Vec::new();
        };
        self.selected_source_columns()
            .into_iter()
            .filter_map(|column| result.columns.get(column).map(|column| column.name.clone()))
            .collect()
    }

    fn run_axis_command(
        &mut self,
        axis: Axis,
        command: AxisCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.axis_menu = None;
        match command {
            AxisCommand::Copy { headers } => self.copy_selection(headers, cx),
            AxisCommand::AutoFit => {
                for column in self.selected_source_columns() {
                    self.auto_size_column(column, window, cx);
                }
            }
            AxisCommand::Sort(descending) => {
                let keys = self
                    .selected_column_names()
                    .into_iter()
                    .map(|field| SortKey {
                        field,
                        descending,
                        nulls_first: false,
                    })
                    .collect();
                self.execute_sort(keys, self.displayed_request.top_n, cx);
            }
            AxisCommand::SortOptions => self.open_sort_controls(self.selected_column_names(), cx),
            AxisCommand::Freeze => {
                if axis == Axis::Rows {
                    self.table_state.pin_rows(self.selected_source_rows());
                } else {
                    let names = self.selected_column_names();
                    let mut candidate = self.table_state.clone();
                    candidate.pin_columns(&names);
                    let required = 52.
                        + self.result.as_ref().map_or(0., |result| {
                            result
                                .columns
                                .iter()
                                .enumerate()
                                .filter(|(_, column)| candidate.is_column_pinned(&column.name))
                                .map(|(index, _)| self.widths.get(index).copied().unwrap_or(150.))
                                .sum::<f32>()
                        });
                    let available = f32::from(window.viewport_size().width)
                        - self.right_sidebar_width(window)
                        - 24.;
                    let limit = (available - 180.).max(0.);
                    if required > limit {
                        self.error = Some(format!(
                            "Cannot freeze these columns: the frozen area needs {required:.0} px, but only {limit:.0} px is available while keeping 180 px for the table. Unfreeze columns, reduce their widths, or close the sidebar."
                        ));
                        cx.notify();
                        return;
                    }
                    self.table_state = candidate;
                }
                self.refresh_table_order(cx);
            }
            AxisCommand::Unfreeze => {
                match axis {
                    Axis::Rows => self.table_state.unpin_rows(self.selected_source_rows()),
                    Axis::Columns => self.table_state.unpin_columns(self.selected_column_names()),
                }
                self.refresh_table_order(cx);
            }
            AxisCommand::UnfreezeAll => {
                match axis {
                    Axis::Rows => self.table_state.clear_row_pins(),
                    Axis::Columns => self.table_state.clear_column_pins(),
                }
                self.refresh_table_order(cx);
            }
            AxisCommand::Mark(color) => match axis {
                Axis::Rows => self
                    .table_state
                    .mark_rows(self.selected_source_rows(), color),
                Axis::Columns => self
                    .table_state
                    .mark_columns(self.selected_column_names(), color),
            },
            AxisCommand::WordWrap(wrap) => {
                for name in self.selected_column_names() {
                    self.formats.entry(name).or_default().word_wrap = wrap;
                }
            }
            AxisCommand::ColumnDisplay { column, binary } => {
                if binary {
                    self.prepare_binary_formats(column, cx);
                }
                self.show_panel(Panel::ColumnDisplay { column, binary }, cx);
            }
        }
        cx.notify();
    }

    pub(super) fn render_axis_menu(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let Some(menu) = self.axis_menu else {
            return div().into_any_element();
        };
        let c = Colors::new(self.preferences.dark);
        // A column selection spans every row. Do not allocate all row IDs just
        // to paint its menu (and vice versa for a whole-row selection).
        // Row marks/pins store individual identities. A whole-file selection can
        // still be copied (within the clipboard budget) without expanding it here.
        let large_row_selection = self.csv_browse.is_some()
            && self
                .selection
                .is_some_and(|(a, b)| a.0.abs_diff(b.0) >= 4096);
        let rows = if menu.axis == Axis::Rows && !large_row_selection {
            self.selected_source_rows()
        } else {
            Vec::new()
        };
        let columns = if menu.axis == Axis::Columns {
            self.selected_column_names()
        } else {
            Vec::new()
        };
        let (any_pinned, all_pinned, has_pins, selected) = match menu.axis {
            Axis::Rows => (
                rows.iter().any(|row| self.table_state.is_row_pinned(*row)),
                !rows.is_empty() && rows.iter().all(|row| self.table_state.is_row_pinned(*row)),
                self.frozen_row_count > 0,
                !rows.is_empty() || large_row_selection,
            ),
            Axis::Columns => (
                columns
                    .iter()
                    .any(|column| self.table_state.is_column_pinned(column)),
                !columns.is_empty()
                    && columns
                        .iter()
                        .all(|column| self.table_state.is_column_pinned(column)),
                self.frozen_count > 0,
                !columns.is_empty(),
            ),
        };
        let mut items = vec![
            AxisMenuItem::action("Copy", AxisCommand::Copy { headers: false }, selected),
            AxisMenuItem::action(
                "Copy with Headers",
                AxisCommand::Copy { headers: true },
                selected,
            ),
            AxisMenuItem::separator(),
        ];
        if menu.axis == Axis::Columns {
            let wrapped = selected
                && columns
                    .iter()
                    .all(|name| self.formats.get(name).is_some_and(|style| style.word_wrap));
            items.extend([
                AxisMenuItem::action("Auto fit selected columns", AxisCommand::AutoFit, selected),
                AxisMenuItem {
                    checked: wrapped,
                    ..AxisMenuItem::action(
                        "Word Wrap",
                        AxisCommand::WordWrap(!wrapped),
                        selected && self.csv_browse.is_none(),
                    )
                },
                AxisMenuItem::action("Sort ascending", AxisCommand::Sort(false), selected),
                AxisMenuItem::action("Sort descending", AxisCommand::Sort(true), selected),
                AxisMenuItem::action("Sort & Top N…", AxisCommand::SortOptions, selected),
                AxisMenuItem::separator(),
            ]);
        }
        items.extend([
            AxisMenuItem::action(
                if menu.axis == Axis::Rows {
                    "Freeze selected rows"
                } else {
                    "Freeze selected columns"
                },
                AxisCommand::Freeze,
                selected && !all_pinned && (menu.axis != Axis::Rows || !large_row_selection),
            ),
            AxisMenuItem::action(
                if menu.axis == Axis::Rows {
                    "Unfreeze selected rows"
                } else {
                    "Unfreeze selected columns"
                },
                AxisCommand::Unfreeze,
                any_pinned,
            ),
            AxisMenuItem::action(
                if menu.axis == Axis::Rows {
                    "Unfreeze all rows"
                } else {
                    "Unfreeze all columns"
                },
                AxisCommand::UnfreezeAll,
                has_pins,
            ),
            AxisMenuItem::separator(),
        ]);
        for &color in MarkColor::all() {
            let checked = match menu.axis {
                Axis::Rows => {
                    !rows.is_empty()
                        && rows
                            .iter()
                            .all(|row| self.table_state.row_mark(*row) == Some(color))
                }
                Axis::Columns => {
                    !columns.is_empty()
                        && columns
                            .iter()
                            .all(|column| self.table_state.column_mark(column) == Some(color))
                }
            };
            items.push(AxisMenuItem {
                label: color.label(),
                command: Some(AxisCommand::Mark(Some(color))),
                enabled: selected && (menu.axis != Axis::Rows || !large_row_selection),
                checked,
                color: Some(color),
            });
        }
        items.push(AxisMenuItem::action(
            "Clear marking",
            AxisCommand::Mark(None),
            selected && (menu.axis != Axis::Rows || !large_row_selection),
        ));
        if columns.len() == 1
            && let Some(column) = self.selected_source_columns().first().copied()
            && let Some(info) = self
                .result
                .as_ref()
                .and_then(|result| result.columns.get(column))
        {
            let display = match info.data_type.as_str() {
                "BLOB" => Some(("Binary Display…", true)),
                "DOUBLE" | "FLOAT" | "REAL" => Some(("Floating Point Display…", false)),
                _ => None,
            };
            if let Some((label, binary)) = display {
                items.push(AxisMenuItem::separator());
                items.push(AxisMenuItem::action(
                    label,
                    AxisCommand::ColumnDisplay { column, binary },
                    true,
                ));
            }
        }
        let content_height = controls::MENU_VERTICAL_INSET
            + items
                .iter()
                .map(|item| if item.command.is_some() { 28. } else { 9. })
                .sum::<f32>();
        let viewport = window.viewport_size();
        let width = 270_f32.min((f32::from(viewport.width) - 16.).max(160.));
        let minimum_top = toolbar::layout(window).height;
        let height = content_height.min((f32::from(viewport.height) - minimum_top - 8.).max(28.));
        let left =
            f32::from(menu.position.x).clamp(8., (f32::from(viewport.width) - width - 8.).max(8.));
        let top = f32::from(menu.position.y).clamp(
            minimum_top,
            (f32::from(viewport.height) - height - 8.).max(minimum_top),
        );
        div()
            .id("axis-menu-backdrop")
            .occlude()
            .absolute()
            .inset_0()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.axis_menu = None;
                    cx.stop_propagation();
                    cx.notify();
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, _, _, cx| {
                    this.axis_menu = None;
                    cx.stop_propagation();
                    cx.notify();
                }),
            )
            .child(
                controls::menu_surface("axis-menu", c)
                    .absolute()
                    .left(px(left))
                    .top(px(top))
                    .w(px(width))
                    .h(px(height))
                    .flex()
                    .flex_col()
                    .overflow_y_scroll()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
                    .children(items.into_iter().enumerate().map(|(index, item)| {
                        let Some(command) = item.command else {
                            return div()
                                .h(px(9.))
                                .flex_shrink_0()
                                .px_2()
                                .flex()
                                .items_center()
                                .child(div().h(px(1.)).w_full().bg(rgb(c.grid)))
                                .into_any_element();
                        };
                        controls::menu_item(
                            ("axis-menu-item", index),
                            c,
                            item.checked,
                            item.enabled,
                        )
                        .child(div().w(px(14.)).flex_shrink_0().when(item.checked, |el| {
                            el.child(icons::glyph(Icon::Check, 14., self.preferences.dark))
                        }))
                        .when_some(item.color, |entry, color| {
                            entry.child(
                                div()
                                    .size(px(12.))
                                    .rounded(px(3.))
                                    .border_1()
                                    .border_color(rgb(c.input_border))
                                    .bg(rgb(color.accent(self.preferences.dark))),
                            )
                        })
                        .child(div().flex_1().truncate().child(t(item.label)))
                        .when(item.enabled, |entry| {
                            entry.on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.run_axis_command(menu.axis, command, window, cx);
                            }))
                        })
                        .into_any_element()
                    })),
            )
            .into_any_element()
    }
}
